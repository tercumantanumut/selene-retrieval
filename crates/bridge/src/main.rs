//! sr-bridge: stdio ↔ named-pipe byte proxy launched by MCP clients
//! (`"command": "...\sr-bridge.exe", "args": ["--client", "claude"]`).
//!
//! 1. Attach through `sr-client` (connect → spawn.lock → `sr-launch` → wait ≤ 10 s → handshake).
//! 2. Copy raw MCP bytes both ways. The MCP session lives in the daemon, so MCP version
//!    negotiation is end to end and bridge/daemon skew is limited to the one handshake line.
//! 3. Exit **immediately** on stdin EOF (VS Code escalates to `taskkill /T /F` after 20 s) or when
//!    the daemon closes the pipe.
//!
//! When the daemon can't be reached or refuses (version skew), the bridge answers the client's
//! first request with a JSON-RPC error carrying the plain user message, so the AI app shows a
//! reason instead of "server disconnected".
//!
//! Must never link LanceDB, ort, fastembed or pdfium (`cargo tree -p sr-bridge`).

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use sr_client::{AttachOptions, attach, naming};
use sr_common::error::Error;
use sr_common::protocol::{ConnKind, PROTO_VERSION};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

/// JSON-RPC error code used for "server unavailable" answers (MCP: implementation-defined range).
const UNAVAILABLE_RPC_CODE: i64 = -32002;
/// After stdin EOF: stop when the daemon is silent this long...
const DRAIN_IDLE: Duration = Duration::from_millis(200);
/// ...or after this long in total.
const DRAIN_MAX: Duration = Duration::from_secs(2);

struct Args {
    client: String,
    data_dir: Option<PathBuf>,
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut client = "other".to_string();
    let mut data_dir = None;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--version" | "-V" => {
                println!(
                    "sr-bridge {} (protocol {PROTO_VERSION})",
                    env!("CARGO_PKG_VERSION")
                );
                return Ok(None);
            }
            "--client" => client = it.next().ok_or("--client needs a value")?,
            "--data-dir" => {
                data_dir = Some(PathBuf::from(it.next().ok_or("--data-dir needs a value")?))
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    // Keep ids short and printable; they only feed plain text and diagnostics.
    client.retain(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    client.truncate(32);
    Ok(Some(Args { client, data_dir }))
}

/// Answers the client's first request (normally `initialize`) with a plain error, then returns.
async fn reply_plain_error(err: &Error) {
    let text = sr_common::protocol::plain_text(&err.user_message());
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    let first_request_id = async {
        // Skip notifications; give up when stdin closes.
        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line)
                && let Some(id) = v.get("id")
            {
                return Some(id.clone());
            }
        }
        None
    };
    let Ok(Some(id)) = tokio::time::timeout(Duration::from_secs(10), first_request_id).await else {
        return;
    };
    let resp = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": UNAVAILABLE_RPC_CODE,
            "message": text,
            "data": err.mcp_structured(),
        }
    });
    let mut out = resp.to_string().into_bytes();
    out.push(b'\n');
    let _ = stdout.write_all(&out).await;
    let _ = stdout.flush().await;
}

async fn run(args: Args) -> u8 {
    let mut opts = AttachOptions::new(ConnKind::Mcp, args.client);
    opts.data_dir = args.data_dir;
    if let Some(p) = naming::dev_env("SR_PROTO_OVERRIDE").and_then(|s| s.to_str()?.parse().ok()) {
        opts.proto = p;
    }
    let conn = match attach(&opts).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("sr-bridge: {e}");
            reply_plain_error(&e).await;
            return 1;
        }
    };
    let (mut pipe_r, mut pipe_w) = tokio::io::split(conn.stream);
    let (closed_tx, mut closed_rx) = tokio::sync::watch::channel(false);
    // stdin → pipe. On EOF (or a dead pipe) signal `down` and park.
    let up = async {
        let mut stdin = tokio::io::stdin();
        let _ = tokio::io::copy(&mut stdin, &mut pipe_w).await;
        let _ = closed_tx.send(true);
        std::future::pending::<()>().await;
    };
    // pipe → stdout, flushed per chunk (clients read line by line). After stdin EOF, keep
    // forwarding responses already in flight, but stop after DRAIN_IDLE without data or DRAIN_MAX
    // in total, so the bridge is gone long before VS Code's 10 s tree-kill escalation.
    let down = async {
        let mut stdout = tokio::io::stdout();
        let mut buf = vec![0u8; 64 * 1024];
        let mut drain_end: Option<tokio::time::Instant> = None;
        loop {
            let n = if let Some(end) = drain_end {
                let left = end.saturating_duration_since(tokio::time::Instant::now());
                match tokio::time::timeout(left.min(DRAIN_IDLE), pipe_r.read(&mut buf)).await {
                    Ok(r) => r?,
                    Err(_) => return Ok::<_, std::io::Error>(()),
                }
            } else {
                tokio::select! {
                    r = pipe_r.read(&mut buf) => r?, // cancel-safe: no bytes lost
                    _ = closed_rx.changed() => {
                        drain_end = Some(tokio::time::Instant::now() + DRAIN_MAX);
                        continue;
                    }
                }
            };
            if n == 0 {
                return Ok(());
            }
            stdout.write_all(&buf[..n]).await?;
            stdout.flush().await?;
        }
    };
    tokio::select! {
        _ = up => 0,
        _ = down => 0, // daemon closed the session, or stdin closed and the drain is done.
    }
}

fn main() -> ExitCode {
    sr_client::harden_dll_search(); // S-08 (M5-E export): before anything can load a DLL by name
    let args = match parse_args() {
        Ok(Some(a)) => a,
        Ok(None) => return ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sr-bridge: {e}");
            return ExitCode::from(2);
        }
    };
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("sr-bridge: {e}");
            return ExitCode::from(1);
        }
    };
    let code = rt.block_on(run(args));
    // Don't wait for tokio's blocking stdin reader thread (it may sit in ReadFile forever).
    std::process::exit(i32::from(code))
}
