//! End to end through the real `sr-daemon.exe` in an install-like layout (exe, `sr-extract.exe`,
//! `models\bge-small-en-v1.5\`): start → second instance exits on the host lock → hidden `admin`
//! CLI adds a folder and waits → an MCP session over the named pipe lists tools, searches
//! (path + lines, progress notifications), gets `-32602` for bad arguments and removed tools,
//! reads `sr://status` → `admin shutdown` drains and exits 0.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sr_client::{AppClient, AttachOptions};
use sr_common::protocol::ConnKind;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn model_dir() -> Option<PathBuf> {
    let d = manifest().join("../providers/models/bge-small-en-v1.5");
    if d.join("model_optimized.onnx").is_file() {
        return Some(d);
    }
    if std::env::var_os("SR_REQUIRE_MODELS").is_some() {
        panic!("SR_REQUIRE_MODELS set but no model in {}", d.display());
    }
    eprintln!("SKIP: no bundled model in {}", d.display());
    None
}

fn worker_exe() -> PathBuf {
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_sr-daemon"));
    let w = exe.with_file_name("sr-extract.exe");
    if !w.is_file() {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let mut cmd = Command::new(cargo);
        cmd.args(["build", "-p", "sr-extract", "--bin", "sr-extract"]);
        if exe
            .parent()
            .and_then(Path::file_name)
            .and_then(|n| n.to_str())
            == Some("release")
        {
            cmd.arg("--release");
        }
        assert!(cmd.status().unwrap().success());
    }
    w
}

/// `<tmp>\app`: sr-daemon.exe + sr-extract.exe + models\bge-small-en-v1.5\*.
fn stage(tmp: &Path, models: &Path) -> PathBuf {
    let app = tmp.join("app");
    let m = app.join("models").join("bge-small-en-v1.5");
    std::fs::create_dir_all(&m).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_sr-daemon"), app.join("sr-daemon.exe")).unwrap();
    std::fs::copy(worker_exe(), app.join("sr-extract.exe")).unwrap();
    for f in std::fs::read_dir(models).unwrap().flatten() {
        if f.path().is_file() {
            std::fs::copy(f.path(), m.join(f.file_name())).unwrap();
        }
    }
    app
}

/// A per-test sandbox profile for the daemon's extract workers (debug hook
/// `SR_APP_CONTAINER_NAME`), deleted on drop, so the tests never create or leak the production
/// `SeleneRetrieval.Extract` profile (M2-I finding).
struct Profile(String);

impl Profile {
    fn new(test: &str) -> Self {
        let name = format!("SeleneRetrieval.Extract.M2T.{test}");
        assert!(name.len() <= 64);
        let _ = sr_extract_client::delete_app_container_profile(&name);
        Self(name)
    }
}

impl Drop for Profile {
    fn drop(&mut self) {
        let _ = sr_extract_client::delete_app_container_profile(&self.0);
    }
}

/// `sr-daemon --data-dir <data> --launched-by test` with the test's sandbox profile.
fn daemon_cmd(app: &Path, data: &Path, profile: &Profile) -> Command {
    let mut c = Command::new(app.join("sr-daemon.exe"));
    c.arg("--data-dir")
        .arg(data)
        .args(["--launched-by", "test"])
        .env("SR_APP_CONTAINER_NAME", &profile.0)
        // Indexing must not wait for a full-screen window on the test desktop (debug-only hook).
        .env("SR_NO_GPU_PAUSE", "1")
        // R-UX30: the tests approve through app-RPC themselves; never open a real sr-app.
        .env("SR_NO_APP_LAUNCH", "1")
        // R31 (M4-E): the test process stands in for sr-app for privileged calls (debug-only hook).
        .env("SR_TRUST_ANY_APP", "1")
        .stdin(Stdio::null());
    c
}

fn admin(app: &Path, data: &Path, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(app.join("sr-daemon.exe"))
        .arg("admin")
        .arg("--data-dir")
        .arg(data)
        .args(args)
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

async fn wait_ready(data: &Path) -> AppClient {
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        let mut o = AttachOptions::new(ConnKind::App, "test");
        o.data_dir = Some(data.to_path_buf());
        o.launch = false;
        o.timeout = Duration::from_secs(2);
        if let Ok(c) = AppClient::connect(&o).await {
            return c;
        }
        assert!(Instant::now() < deadline, "daemon did not open its pipe");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

struct Mcp {
    r: BufReader<tokio::io::ReadHalf<tokio::net::windows::named_pipe::NamedPipeClient>>,
    w: tokio::io::WriteHalf<tokio::net::windows::named_pipe::NamedPipeClient>,
    notes: Vec<Value>,
}

impl Mcp {
    async fn send(&mut self, v: Value) {
        let mut line = v.to_string();
        line.push('\n');
        self.w.write_all(line.as_bytes()).await.unwrap();
        self.w.flush().await.unwrap();
    }
    async fn call(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        loop {
            let mut line = String::new();
            let n = tokio::time::timeout(Duration::from_secs(120), self.r.read_line(&mut line))
                .await
                .expect("MCP reply timed out")
                .unwrap();
            assert!(n > 0, "pipe closed");
            let v: Value = serde_json::from_str(line.trim()).unwrap();
            if v.get("id") == Some(&json!(id)) {
                return v;
            }
            self.notes.push(v);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_admin_and_mcp_over_the_pipe() {
    let Some(models) = model_dir() else { return };
    let t = tempfile::Builder::new().prefix("w2e").tempdir().unwrap();
    let app = stage(t.path(), &models);
    let data = t.path().join("data");
    let corpus = t.path().join("corpus");
    std::fs::create_dir_all(corpus.join("src")).unwrap();
    std::fs::write(
        corpus.join("src").join("billing.rs"),
        "// invoices\n\n/// Computes the late fee for an overdue invoice.\npub fn overdue_penalty_fee(days: u32) -> u32 {\n    days * 7\n}\n",
    )
    .unwrap();
    std::fs::write(
        corpus.join("readme.md"),
        "# Readme\n\nThis project prints greetings.\n",
    )
    .unwrap();
    std::fs::write(corpus.join(".env"), "SECRET=hunter2\n").unwrap();
    // docs/06 change 4: an exact copy is one hit with `also_at`.
    std::fs::create_dir_all(corpus.join("backup")).unwrap();
    std::fs::copy(
        corpus.join("src").join("billing.rs"),
        corpus.join("backup").join("billing.rs"),
    )
    .unwrap();

    let profile = Profile::new("pipe");
    let mut daemon = daemon_cmd(&app, &data, &profile).spawn().unwrap();
    let client = wait_ready(&data).await;
    let ping = client.ping().await.unwrap();
    assert_eq!(ping.daemon_pid, daemon.id());

    // A second daemon for the same data dir exits quietly on the host lock.
    let second = daemon_cmd(&app, &data, &profile).status().unwrap();
    assert!(second.success(), "second instance must exit 0");

    // Hidden admin CLI: add the folder and wait for indexing.
    let (ok, out, err) = admin(
        &app,
        &data,
        &["folder", "add", &corpus.to_string_lossy(), "--wait"],
    );
    assert!(ok, "admin failed: {err}");
    let folders: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(folders[0]["files_indexed"], 3, "{out}");
    assert_eq!(folders[0]["status"], "synced", "{out}");

    // MCP over the pipe (the bridge's path: handshake kind=mcp, then raw JSON-RPC lines).
    let mut o = AttachOptions::new(ConnKind::Mcp, "claude");
    o.data_dir = Some(data.clone());
    o.launch = false;
    let conn = sr_client::attach(&o).await.unwrap();
    let (r, w) = tokio::io::split(conn.stream);
    let mut m = Mcp {
        r: BufReader::new(r),
        w,
        notes: Vec::new(),
    };
    let init = m
        .call(
            1,
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {},
                   "clientInfo": {"name": "w2e-test", "version": "0"}}),
        )
        .await;
    assert_eq!(
        init["result"]["serverInfo"]["name"], "selene-retrieval",
        "{init}"
    );
    m.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .await;
    let tools = m.call(2, "tools/list", json!({})).await;
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert_eq!(names, ["context_search", "list_folders"]);

    let res = m
        .call(
            3,
            "tools/call",
            json!({"name": "context_search", "arguments": {"query": "late fee for overdue invoices", "maxResults": 5},
                   "_meta": {"progressToken": "p-search"}}),
        )
        .await;
    let sc = &res["result"]["structuredContent"];
    let hits = &sc["hits"];
    let first = &hits[0];
    // docs/06 change 2: the root once in `folders`, folder-relative paths in the rows.
    assert_eq!(sc["folders"].as_array().unwrap().len(), 1, "{res}");
    let root = sc["folders"][0]["root"].as_str().unwrap().to_string();
    assert!(root.ends_with("corpus"), "{res}");
    assert_eq!(first["path"], "src/billing.rs", "{res}");
    assert!(
        first.get("f").is_none() && first.get("score").is_none(),
        "{res}"
    );
    assert!(
        first["also_at"][0]
            .as_str()
            .unwrap()
            .starts_with("backup/billing.rs:"),
        "{res}"
    );
    assert!(
        !hits
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h["path"] == "backup/billing.rs"),
        "the copy is not a second hit: {res}"
    );
    assert!(first["line_start"].as_u64().unwrap() >= 1);
    assert!(first["line_end"].as_u64().unwrap() >= first["line_start"].as_u64().unwrap());
    assert!(
        m.notes
            .iter()
            .any(|n| n["method"] == "notifications/progress"
                && n["params"]["progressToken"] == "p-search"),
        "progress notifications: {:?}",
        m.notes
    );
    // Secrets never indexed.
    let res = m
        .call(
            4,
            "tools/call",
            json!({"name": "context_search", "arguments": {"query": "hunter2"}}),
        )
        .await;
    // The secret's content and file never come back (the reply may echo the query itself:
    // R-UX18 answers "Nothing closely matched “hunter2” …" with zero hits).
    let text = res["result"].to_string();
    assert!(
        !text.contains("SECRET=") && !text.contains(".env"),
        "{text}"
    );
    assert_eq!(
        res["result"]["structuredContent"]["hits"],
        json!([]),
        "{text}"
    );
    assert!(res["result"]["structuredContent"].get("count").is_none());
    assert_eq!(
        res["result"]["structuredContent"]["no_good_match"], true,
        "{text}"
    );

    // Bad arguments are JSON-RPC -32602.
    let res = m
        .call(
            8,
            "tools/call",
            json!({"name": "context_search", "arguments": {"maxResults": 3}}),
        )
        .await;
    assert_eq!(res["error"]["code"], -32602, "{res}");
    // Amendment 16: removed tools are unknown names, nothing is read.
    let p = first["path"].as_str().unwrap().to_string();
    for (id, name, args) in [
        (91, "read_file", json!({"path": p})),
        (92, "grep", json!({"pattern": "overdue_penalty_fee"})),
        (93, "get_status", json!({})),
        (94, "search", json!({"query": "late fee"})),
    ] {
        let res = m
            .call(id, "tools/call", json!({"name": name, "arguments": args}))
            .await;
        assert_eq!(res["error"]["code"], -32602, "{name}: {res}");
    }
    let res = m
        .call(10, "resources/read", json!({"uri": "sr://status"}))
        .await;
    let status: Value =
        serde_json::from_str(res["result"]["contents"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(status["summary"], "Up to date", "{status}");
    drop(m);

    // Drain + exit through the admin CLI (the updater's path).
    let (ok, _, err) = admin(&app, &data, &["shutdown"]);
    assert!(ok, "{err}");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(st) = daemon.try_wait().unwrap() {
            assert!(st.success(), "daemon exit {st:?}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "daemon did not exit after shutdown"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[test]
fn refuses_a_data_dir_that_breaks_the_path_rule() {
    let exe = env!("CARGO_BIN_EXE_sr-daemon");
    let st = Command::new(exe)
        .args(["--data-dir", r"\\server\share\sr"])
        .status()
        .unwrap();
    assert_eq!(st.code(), Some(3));
    let st = Command::new(exe).arg("--bogus").status().unwrap();
    assert_eq!(st.code(), Some(2));
}

/// Amendment 4.8 / m1-d O1: the GPU DLLs are delay-loaded, never load-time imports, so a planted
/// `DirectML.dll` in the install dir cannot load before `main` hardens the DLL search path.
#[test]
fn daemon_delay_loads_the_gpu_dlls() {
    let Ok(out) = Command::new("dumpbin")
        .args(["-nologo", "-dependents", env!("CARGO_BIN_EXE_sr-daemon")])
        .output()
    else {
        eprintln!("SKIP: dumpbin not on PATH (run from an MSVC environment)");
        return;
    };
    let text = String::from_utf8_lossy(&out.stdout).to_ascii_lowercase();
    let (normal, delayed) = text
        .split_once("delay load dependencies")
        .expect("no delay-load section");
    for dll in ["directml.dll", "d3d12.dll", "dxgi.dll"] {
        assert!(
            !normal.contains(dll),
            "{dll} is a load-time import:\n{text}"
        );
        assert!(delayed.contains(dll), "{dll} not delay-loaded:\n{text}");
    }
}

/// Amendment 4.24 (M1 tester item B) through the real daemon: kill -9 with a pending outbox,
/// restart, and the pipe answers `initialize` within the readiness budget although the backlog
/// still has to be embedded; search answers during the catch-up; the backlog converges.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_after_kill_with_a_pending_outbox_is_ready_at_once() {
    let Some(models) = model_dir() else { return };
    let t = tempfile::Builder::new().prefix("m2r").tempdir().unwrap();
    let app = stage(t.path(), &models);
    let data = t.path().join("data");
    let corpus = t.path().join("corpus");
    let n_files = 200usize;
    std::fs::create_dir_all(&corpus).unwrap();
    for i in 0..n_files {
        std::fs::write(
            corpus.join(format!("note{i:04}.md")),
            format!("# Note {i}\n\nuniqword{i} is described only in note number {i}.\n"),
        )
        .unwrap();
    }
    let profile = Profile::new("restart");
    let spawn = || daemon_cmd(&app, &data, &profile).spawn().unwrap();
    let mut daemon = spawn();
    drop(wait_ready(&data).await);
    let (ok, _, err) = admin(&app, &data, &["folder", "add", &corpus.to_string_lossy()]);
    assert!(ok, "{err}");
    // Wait until every file reached T1 (no job left), then crash before the writer catches up.
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let (ok, out, _) = admin(&app, &data, &["status"]);
        if ok {
            let s: Value = serde_json::from_str(&out).unwrap_or(Value::Null);
            let v = &s["value"];
            if v["queued_jobs"] == 0 && v["folders"][0]["files_total"] == n_files {
                break;
            }
        }
        assert!(Instant::now() < deadline, "T1 did not finish");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    daemon.kill().unwrap();
    let _ = daemon.wait();

    // Restart: time from spawn to the `initialize` reply.
    let (budget, load) = ready_budget(&app);
    eprintln!("readiness budget {budget:?} ({load})");
    let t0 = Instant::now();
    let mut daemon = spawn();
    let conn = loop {
        let mut o = AttachOptions::new(ConnKind::Mcp, "claude");
        o.data_dir = Some(data.clone());
        o.launch = false;
        o.timeout = Duration::from_millis(200);
        if let Ok(c) = sr_client::attach(&o).await {
            break c;
        }
        assert!(t0.elapsed() < Duration::from_secs(60), "pipe never opened");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let (r, w) = tokio::io::split(conn.stream);
    let mut m = Mcp {
        r: BufReader::new(r),
        w,
        notes: Vec::new(),
    };
    let init = m
        .call(
            1,
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {},
                   "clientInfo": {"name": "m2r-test", "version": "0"}}),
        )
        .await;
    let ready = t0.elapsed();
    assert_eq!(init["result"]["serverInfo"]["name"], "selene-retrieval");
    eprintln!("initialize answered {ready:?} after the restart");
    // Where the start time went (step timestamps from the daemon log). Since O1 the pipe answers
    // before the engine is up, so wait until the restart has logged its own `ready` (the log holds
    // both starts); otherwise the gap below measures the first start (fresh install-dir ACL grant).
    let steps_deadline = Instant::now() + Duration::from_secs(60);
    let steps = loop {
        let mut steps = String::new();
        for f in std::fs::read_dir(data.join("logs"))
            .into_iter()
            .flatten()
            .flatten()
        {
            let text = std::fs::read_to_string(f.path()).unwrap_or_default();
            for l in text.lines().filter(|l| {
                l.contains("sr_engine::start") || l.contains("ready") || l.contains("model loaded")
            }) {
                steps.push_str(l);
                steps.push('\n');
            }
        }
        if steps.matches("step=\"ready\"").count() >= 2 || Instant::now() >= steps_deadline {
            break steps;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    eprintln!("{steps}");
    // Amendment 4.24: with the backlog pending, everything after the model load (stores,
    // watermark copy, finalize_applied, roots, tier) is cheap. Process start, the data-dir ACL
    // and the model load (1.3-1.5 s for a DirectML session on a loaded box) come before it and
    // are outside 4.24; the hard cap guards against any replay creeping back in.
    let after_model = step_gap(&steps, "step=\"embedder\"", "step=\"ready\"");
    eprintln!("embedder -> ready: {after_model:?}");
    assert!(
        after_model.is_some_and(|d| d <= Duration::from_millis(500)),
        "start steps after the model took {after_model:?}\n{steps}"
    );
    assert!(
        ready <= budget,
        "initialize took {ready:?} (budget {budget:?})\n{steps}"
    );
    m.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .await;
    // Search answers during the catch-up (last committed state).
    let res = m
        .call(
            2,
            "tools/call",
            json!({"name": "context_search", "arguments": {"query": "uniqword7", "maxResults": 3}}),
        )
        .await;
    assert_ne!(res["result"]["isError"], true, "{res}");
    let res = m
        .call(3, "resources/read", json!({"uri": "sr://status"}))
        .await;
    let status: Value =
        serde_json::from_str(res["result"]["contents"][0]["text"].as_str().unwrap()).unwrap();
    eprintln!("status during catch-up: {}", status["summary"]);
    if !status["catch_up"].is_null() {
        assert!(
            status["summary"]
                .as_str()
                .unwrap()
                .starts_with("Catching up after restart"),
            "{status}"
        );
    }
    drop(m);
    // The background writer converges.
    let (ok, out, err) = admin(&app, &data, &["wait-idle", "--timeout-ms", "600000"]);
    assert!(
        ok && out.contains("\"idle\": true"),
        "wait-idle: {out} {err}"
    );
    let (ok, out, err) = admin(&app, &data, &["folder", "list"]);
    assert!(ok, "{err}");
    let folders: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(folders[0]["files_indexed"], n_files, "{out}");
    let (ok, out, _) = admin(&app, &data, &["search", "uniqword123", "--max", "3"]);
    assert!(ok && out.contains("note0123.md"), "{out}");
    let (ok, _, err) = admin(&app, &data, &["shutdown"]);
    assert!(ok, "{err}");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(st) = daemon.try_wait().unwrap() {
            assert!(st.success(), "daemon exit {st:?}");
            break;
        }
        assert!(Instant::now() < deadline, "daemon did not exit");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Time between the last log lines containing `from` and `to` (RFC 3339 timestamps, UTC).
fn step_gap(log: &str, from: &str, to: &str) -> Option<Duration> {
    fn secs(line: &str) -> Option<f64> {
        // 2026-10-18T09:28:29.709173Z -> seconds of the day
        let t = line
            .split_whitespace()
            .next()?
            .split('T')
            .nth(1)?
            .trim_end_matches('Z');
        let mut it = t.split(':');
        let h: f64 = it.next()?.parse().ok()?;
        let m: f64 = it.next()?.parse().ok()?;
        let s: f64 = it.next()?.parse().ok()?;
        Some(h * 3600.0 + m * 60.0 + s)
    }
    let a = log
        .lines()
        .rev()
        .find(|l| l.contains(from))
        .and_then(secs)?;
    let b = log.lines().rev().find(|l| l.contains(to)).and_then(secs)?;
    (b >= a).then(|| Duration::from_secs_f64(b - a))
}

/// Readiness budget for spawn → `initialize` (M2 tester O1): **2 s on an idle machine**, **5 s on
/// a loaded one**. "Loaded" = the fastest of three trivial `sr-daemon --version` spawns takes over
/// 250 ms (process start alone is then slow). `SR_TEST_READY_BUDGET_MS` overrides both.
fn ready_budget(app: &Path) -> (Duration, &'static str) {
    if let Some(ms) = std::env::var("SR_TEST_READY_BUDGET_MS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        return (Duration::from_millis(ms), "override");
    }
    let mut best = Duration::MAX;
    for _ in 0..3 {
        let t = Instant::now();
        let ok = Command::new(app.join("sr-daemon.exe"))
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok, "sr-daemon --version failed");
        best = best.min(t.elapsed());
    }
    eprintln!("process start calibration: {best:?}");
    if best > Duration::from_millis(250) {
        (Duration::from_millis(5000), "loaded")
    } else {
        (Duration::from_millis(2000), "idle")
    }
}

async fn mcp_connect(data: &Path, deadline: Instant) -> Mcp {
    loop {
        let mut o = AttachOptions::new(ConnKind::Mcp, "claude");
        o.data_dir = Some(data.to_path_buf());
        o.launch = false;
        o.timeout = Duration::from_millis(200);
        if let Ok(c) = sr_client::attach(&o).await {
            let (r, w) = tokio::io::split(c.stream);
            return Mcp {
                r: BufReader::new(r),
                w,
                notes: Vec::new(),
            };
        }
        assert!(Instant::now() < deadline, "pipe never opened");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn wait_exit(daemon: &mut std::process::Child) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(st) = daemon.try_wait().unwrap() {
            assert!(st.success(), "daemon exit {st:?}");
            return;
        }
        assert!(Instant::now() < deadline, "daemon did not exit");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// O1: the pipe opens before the model load and GPU detection, so a **cold** spawn (fresh data
/// dir, no settings file) answers MCP `initialize` within the budget; `tools/list` and
/// `sr://status` answer at once ("Starting…" or ready); `context_search` waits for the engine and
/// then answers. The MCP `add_folder` and `sync_folder` tools are not listed by default, and
/// calling them anyway gets the unknown-tool answer, nothing recorded (P1.2, Amendment 16).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cold_spawn_answers_initialize_within_the_budget() {
    let Some(models) = model_dir() else { return };
    let t = tempfile::Builder::new().prefix("m2t").tempdir().unwrap();
    let app = stage(t.path(), &models);
    let data = t.path().join("data");
    let (budget, load) = ready_budget(&app);
    let profile = Profile::new("cold");
    let t0 = Instant::now();
    let mut daemon = daemon_cmd(&app, &data, &profile).spawn().unwrap();
    let mut m = mcp_connect(&data, t0 + Duration::from_secs(60)).await;
    let attached = t0.elapsed();
    let init = m
        .call(
            1,
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {},
                   "clientInfo": {"name": "m2t", "version": "0"}}),
        )
        .await;
    let ready = t0.elapsed();
    assert_eq!(init["result"]["serverInfo"]["name"], "selene-retrieval");
    eprintln!(
        "cold spawn: pipe attached {attached:?}, initialize answered {ready:?} (budget {budget:?}, {load})"
    );
    assert!(
        ready <= budget,
        "initialize took {ready:?} (budget {budget:?}, machine {load})"
    );
    m.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .await;
    let tl = m.call(2, "tools/list", json!({})).await;
    let names: Vec<&str> = tl["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert_eq!(names, ["context_search", "list_folders"], "{tl}");
    let st = m
        .call(3, "resources/read", json!({"uri": "sr://status"}))
        .await;
    assert!(st["result"]["contents"][0]["text"].is_string(), "{st}");
    let status: Value =
        serde_json::from_str(st["result"]["contents"][0]["text"].as_str().unwrap()).unwrap();
    eprintln!("status right after initialize: {}", status["summary"]);
    // Search waits for the engine (model load), then answers: no folders yet.
    let s = m
        .call(
            4,
            "tools/call",
            json!({"name": "context_search", "arguments": {"query": "anything"}}),
        )
        .await;
    assert_ne!(s["result"]["isError"], true, "{s}");
    eprintln!("first search answered {:?} after spawn", t0.elapsed());
    // add_folder and sync_folder are off: unknown tools (-32602), like any unknown name.
    for (id, name, args) in [
        (5, "add_folder", json!({"path": t.path().to_string_lossy()})),
        (6, "sync_folder", json!({})),
    ] {
        let a = m
            .call(id, "tools/call", json!({"name": name, "arguments": args}))
            .await;
        assert_eq!(a["error"]["code"], -32602, "{name}: {a}");
        assert!(a.get("result").is_none(), "{name}: {a}");
    }
    // Guards "nothing was queued" (the `-32602` checks above prove the gate).
    let client = wait_ready(&data).await;
    let list = client.request("approvals.list", json!({})).await.unwrap();
    assert_eq!(list, json!([]));
    drop(client);
    drop(m);
    let (ok, _, err) = admin(&app, &data, &["shutdown"]);
    assert!(ok, "{err}");
    wait_exit(&mut daemon);
}

/// Approvals (M2 exit; Q5): with `[integrations] mcp_add_folder_enabled = true` the MCP tool is
/// listed (and `sync_folder` with `mcp_sync_folder_enabled = true`, Amendment 16); a call records a pending request the agent is told to wait on (nothing is added); the
/// app lists it and the user's approval adds the folder through the normal path (first scan).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mcp_add_folder_needs_the_users_approval_when_turned_on() {
    let Some(models) = model_dir() else { return };
    let t = tempfile::Builder::new().prefix("m2t").tempdir().unwrap();
    let app = stage(t.path(), &models);
    let data = t.path().join("data");
    let corpus = t.path().join("asked");
    std::fs::create_dir_all(&corpus).unwrap();
    std::fs::write(
        corpus.join("note.md"),
        "# Note\n\nzebracornflake lives here.\n",
    )
    .unwrap();
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(
        data.join("settings.toml"),
        "[integrations]\nmcp_add_folder_enabled = true\nmcp_sync_folder_enabled = true\n",
    )
    .unwrap();
    let profile = Profile::new("approve");
    let mut daemon = daemon_cmd(&app, &data, &profile).spawn().unwrap();
    let mut m = mcp_connect(&data, Instant::now() + Duration::from_secs(60)).await;
    m.call(
        1,
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": {},
               "clientInfo": {"name": "m2t", "version": "0"}}),
    )
    .await;
    m.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .await;
    let tl = m.call(2, "tools/list", json!({})).await;
    assert!(tl.to_string().contains("\"add_folder\""), "{tl}");
    // The settings file turns `sync_folder` on too: listed (from the pre-start hint or the engine).
    assert!(tl.to_string().contains("\"sync_folder\""), "{tl}");
    let a = m
        .call(
            3,
            "tools/call",
            json!({"name": "add_folder", "arguments": {"path": corpus.to_string_lossy()}}),
        )
        .await;
    assert_ne!(a["result"]["isError"], true, "{a}");
    assert_eq!(
        a["result"]["structuredContent"]["status"], "pending_approval",
        "{a}"
    );
    let text = a["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("approve"), "{text}");
    // Nothing added yet.
    let (ok, out, _) = admin(&app, &data, &["folder", "list"]);
    assert!(ok && out.trim() == "[]", "{out}");
    // A refused folder is refused before anyone is asked.
    let r = m
        .call(
            4,
            "tools/call",
            json!({"name": "add_folder", "arguments": {"path": t.path().join("missing").to_string_lossy()}}),
        )
        .await;
    assert_eq!(r["result"]["isError"], true, "{r}");
    // The app sees the pending request and the user approves it.
    let client = wait_ready(&data).await;
    let list = client.request("approvals.list", json!({})).await.unwrap();
    assert_eq!(list.as_array().map(Vec::len), Some(1), "{list}");
    let id = list[0]["approval_id"].as_str().unwrap().to_owned();
    assert!(
        list[0]["path"].as_str().unwrap().ends_with("asked"),
        "{list}"
    );
    let ok = client
        .request("approvals.approve", json!({"approval_id": id}))
        .await
        .unwrap();
    assert!(ok["folder_id"].is_string(), "{ok}");
    let list = client.request("approvals.list", json!({})).await.unwrap();
    assert_eq!(list, json!([]));
    let (ok, out, err) = admin(&app, &data, &["wait-idle", "--timeout-ms", "300000"]);
    assert!(
        ok && out.contains("\"idle\": true"),
        "wait-idle: {out} {err}"
    );
    let (ok, out, _) = admin(&app, &data, &["search", "zebracornflake", "--max", "3"]);
    assert!(ok && out.contains("note.md"), "{out}");
    drop(m);
    drop(client);
    let (ok, _, err) = admin(&app, &data, &["shutdown"]);
    assert!(ok, "{err}");
    wait_exit(&mut daemon);
}

/// Wait for the daemon to exit by itself; its exit code.
fn exit_code_within(daemon: &mut std::process::Child, secs: u64) -> Option<i32> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(st) = daemon.try_wait().unwrap() {
            return st.code();
        }
        if Instant::now() > deadline {
            let _ = daemon.kill();
            return None;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn missing_index_daemon(data: &Path) -> std::process::Child {
    Command::new(env!("CARGO_BIN_EXE_sr-daemon"))
        .arg("--data-dir")
        .arg(data)
        .args(["--launched-by", "test"])
        .env("SR_TRUST_ANY_APP", "1")
        .env("SR_NO_APP_LAUNCH", "1")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

/// R34 (M5-E): the moved index's folder is gone → the service waits in a recovery mode on the
/// pipe (plain status, `IndexMissing` for engine calls, `index_location.recover`) instead of
/// exiting 3 with the added-folder sentence; "Start fresh" restarts it on an empty index, and
/// reconnecting the drive restarts it by itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_moved_index_waits_in_recovery_mode() {
    use sr_common::error::ErrorCode;
    let t = tempfile::tempdir().unwrap();
    let data = t.path().join("d");
    std::fs::create_dir_all(&data).unwrap();
    let gone = t.path().join("gone-index");
    let pointer = json!({ "path": gone }).to_string();
    std::fs::write(data.join("index-location.json"), &pointer).unwrap();
    let backup = json!({ "version": 1, "folders": [{ "path": r"C:\data\notes", "settings": {} }] });
    std::fs::write(data.join("folders-backup.json"), backup.to_string()).unwrap();

    let mut daemon = missing_index_daemon(&data);
    let client = wait_ready(&data).await;
    let st = client.request("engine.status", json!({})).await.unwrap();
    assert_eq!(st["value"]["summary"], "Search index not found", "{st}");
    assert_eq!(st["value"]["index_missing"]["folders_to_restore"], 1);
    assert!(st["value"]["index_missing"]["old_copy"].is_null());
    let e = client.request("folders.list", json!({})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::IndexMissing);
    assert!(!e.user_message().agent_text().contains("add the folder"));
    let loc = client
        .request("index_location.status", json!({}))
        .await
        .unwrap();
    assert!(loc["missing"].is_object(), "{loc}");
    // "Move it back" needs an earlier copy; there is none here.
    let e = client
        .request("index_location.recover", json!({ "choice": "move_back" }))
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound);
    // "Start fresh": the service restarts (exit 0) on an empty index in the default place.
    client
        .request("index_location.recover", json!({ "choice": "start_fresh" }))
        .await
        .unwrap();
    drop(client);
    assert_eq!(exit_code_within(&mut daemon, 60), Some(0));
    assert!(!data.join("index-location.json").exists());
    assert!(data.join("restore-folders.json").is_file());
    assert!(!gone.exists(), "nothing is created at the missing place");

    // The drive comes back while the service waits: it restarts by itself.
    std::fs::write(data.join("index-location.json"), &pointer).unwrap();
    let mut daemon = missing_index_daemon(&data);
    let client = wait_ready(&data).await;
    drop(client);
    std::fs::create_dir_all(&gone).unwrap();
    assert_eq!(exit_code_within(&mut daemon, 60), Some(0));
}
