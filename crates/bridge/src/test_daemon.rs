//! sr-test-daemon: test stub standing in for `sr-daemon` until the engine is composed.
//! **Never packaged.** It compiles the real pipe server (`crates/engine/src/pipe.rs`) over a fake
//! service, and follows the daemon start contract the real `sr-daemon` must keep:
//!
//! `--data-dir <dir> --launched-by <breakaway|task|injob|...>`: host.lock (held → exit 0) →
//! refuse packaged context (exit 4) → open the pipe last (= ready) → serve until drained.
//!
//! Fake service: MCP = one JSON response line per JSON-RPC request line (enough to test a byte
//! proxy); app-RPC `test.emit {n}` publishes n events, `test.info` describes the process.
//! Test hooks: `--proto N`, `--ring N`, `--idle-exit-secs N` (default 120, so a leaked stub dies),
//! `--skip-host-lock`, `--strict-trust` (no self-trust: the real S-05 / R50 caller rules).
#![windows_subsystem = "windows"]

// pipe.rs logs refused callers with `tracing::warn!` (M5-E, S-05). sr-bridge keeps its tiny
// dependency graph (no `tracing`, `xtask check-bridge-deps`), so the stub resolves `tracing::` to
// this crate and its no-op `warn!` below.
extern crate self as tracing;
/// No-op stand-in for `tracing::warn!` (the stub has no logging).
#[macro_export]
macro_rules! warn {
    ($($t:tt)*) => {};
}

#[allow(dead_code)]
#[path = "../../engine/src/pipe.rs"]
mod pipe;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use pipe::{EventHub, PipeServerConfig, PipeService, SessionInfo};
use serde_json::{Value, json};
use sr_client::sys::ServerStream;
use sr_common::error::{Error, ErrorCode};
use sr_common::events::{EngineEvent, Snapshot};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

struct Stub {
    hub: EventHub<EngineEvent>,
    emitted: AtomicU64,
    launched_by: String,
    in_job: Option<bool>,
    data_dir: PathBuf,
}

fn log(dd: &Path, line: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dd.join("daemon.log"))
    {
        let _ = f.write_all(
            format!(
                "{} pid={} {line}\n",
                sr_common::events::now_ms(),
                std::process::id()
            )
            .as_bytes(),
        );
    }
}

impl Stub {
    fn info(&self) -> Value {
        json!({
            "daemon_pid": std::process::id(),
            "launched_by": self.launched_by,
            "in_job": self.in_job,
            "data_dir": self.data_dir,
            "cursor": self.hub.cursor(),
        })
    }
}

impl PipeService for Stub {
    type Event = EngineEvent;

    fn events(&self) -> &EventHub<EngineEvent> {
        &self.hub
    }

    fn snapshot(&self) -> Snapshot<Value> {
        self.hub
            .snapshot(|| json!({"emitted": self.emitted.load(Ordering::SeqCst)}))
    }

    async fn serve_mcp(&self, stream: ServerStream, _s: SessionInfo) {
        let (r, mut w) = tokio::io::split(stream);
        let mut lines = BufReader::new(r).lines();
        log(
            &self.data_dir,
            &format!("mcp session {} pid={:?}", _s.id, _s.client_pid),
        );
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(req) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let Some(id) = req.get("id").cloned() else {
                continue; // notification
            };
            let mut result = self.info();
            result["method"] = req.get("method").cloned().unwrap_or(Value::Null);
            if req["method"] == "initialize" {
                result["protocolVersion"] = req["params"]["protocolVersion"].clone();
                result["serverInfo"] = json!({"name": "sr-test-daemon", "version": "0"});
            }
            let mut out = json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string();
            out.push('\n');
            if w.write_all(out.as_bytes()).await.is_err() || w.flush().await.is_err() {
                break;
            }
        }
        log(&self.data_dir, &format!("mcp session {} end", _s.id));
    }

    async fn call(&self, method: String, params: Value, _s: SessionInfo) -> Result<Value, Error> {
        match method.as_str() {
            "test.emit" => {
                let n = params["n"].as_u64().unwrap_or(1);
                let mut last = 0;
                for _ in 0..n {
                    let i = self.emitted.fetch_add(1, Ordering::SeqCst) + 1;
                    last = self
                        .hub
                        .publish(EngineEvent::QueueDepth {
                            queued: i,
                            leased: 0,
                        })
                        .seq;
                }
                Ok(json!({"last_seq": last}))
            }
            "test.info" => Ok(self.info()),
            _ => {
                Err(Error::new(ErrorCode::InvalidInput).with_detail(format!("no method {method}")))
            }
        }
    }

    async fn drain(&self) {
        log(&self.data_dir, "drain");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn arg(args: &[String], k: &str) -> Option<String> {
    args.iter()
        .position(|a| a == k)
        .and_then(|i| args.get(i + 1).cloned())
}

fn main() {
    // S-05 (M5): pipe.rs refuses `host.shutdown` and handshake drain-restarts from callers that
    // are not our shipped programs. This never-packaged stub tests attach / events / skew with
    // the test exe as the caller, and the scheduled-task launch doesn't pass the test's
    // environment on, so the stub trusts its callers itself (debug-only hook, same as tests).
    // The trust rules are tested in sr-engine (pipe unit tests, tests/m4.rs).
    #[allow(unsafe_code)]
    // SAFETY: first statement of main, before any thread exists; nothing reads the env yet.
    unsafe {
        std::env::set_var("SR_TRUST_ANY_APP", "1");
    }
    // `--strict-trust`: the real caller rules (the socket tests check that an unproven caller is
    // refused).
    if std::env::args().any(|a| a == "--strict-trust") {
        #[allow(unsafe_code)]
        // SAFETY: still before any thread exists.
        unsafe {
            std::env::remove_var("SR_TRUST_ANY_APP");
        }
    }
    let args: Vec<String> = std::env::args().collect();
    let Some(data_dir) = arg(&args, "--data-dir").map(PathBuf::from) else {
        std::process::exit(2);
    };
    let _ = std::fs::create_dir_all(&data_dir);
    let launched_by = arg(&args, "--launched-by").unwrap_or_else(|| "direct".into());
    let in_job = sr_client::sys::current_process_in_job();
    let pid = std::process::id();
    let _ = std::fs::write(
        data_dir.join(format!("start-{pid}.json")),
        json!({"pid": pid, "launched_by": launched_by, "in_job": in_job}).to_string(),
    );
    log(
        &data_dir,
        &format!("start launched_by={launched_by} in_job={in_job:?}"),
    );

    // 1. One host per user + data dir.
    let _host = if args.iter().any(|a| a == "--skip-host-lock") {
        None
    } else {
        match pipe::HostLock::acquire(&data_dir) {
            Ok(Some(l)) => Some(l),
            Ok(None) => {
                log(&data_dir, "host.lock held; exiting");
                std::process::exit(0);
            }
            Err(e) => {
                log(&data_dir, &format!("host.lock error {e}"));
                std::process::exit(3);
            }
        }
    };
    // 2. Never own stores with package identity.
    if let Some(e) = pipe::packaged_context_error() {
        log(&data_dir, &format!("{e}"));
        std::process::exit(4);
    }
    let mut cfg = PipeServerConfig::new(data_dir.clone());
    if let Some(p) = arg(&args, "--proto").and_then(|s| s.parse().ok()) {
        cfg.proto = p;
    }
    let ring = arg(&args, "--ring")
        .and_then(|s| s.parse().ok())
        .unwrap_or(sr_common::events::DEFAULT_RING_CAPACITY);
    let idle = Duration::from_secs(
        arg(&args, "--idle-exit-secs")
            .and_then(|s| s.parse().ok())
            .unwrap_or(120),
    );
    let svc = Arc::new(Stub {
        hub: EventHub::new(ring),
        emitted: AtomicU64::new(0),
        launched_by,
        in_job,
        data_dir: data_dir.clone(),
    });
    let Ok(rt) = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    else {
        std::process::exit(5);
    };
    let code = rt.block_on(async move {
        // 3. Pipe last: an open pipe means ready.
        let h = match pipe::serve(cfg, svc) {
            Ok(h) => h,
            Err(e) => {
                log(&data_dir, &format!("pipe create failed: {e}; exiting"));
                return 0;
            }
        };
        let _ = std::fs::write(data_dir.join(format!("owner-{pid}")), h.effective_sddl());
        log(
            &data_dir,
            &format!("pipe ready {} sddl={}", h.pipe_name(), h.effective_sddl()),
        );
        let h2 = h.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;
                if h2.sessions() == 0 && h2.idle_for() > idle {
                    h2.begin_drain();
                    return;
                }
            }
        });
        h.stopped().await;
        log(&data_dir, "stopped");
        0
    });
    std::process::exit(code);
}
