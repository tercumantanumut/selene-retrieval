//! sr-daemon (M0-D spike): rmcp server over a user-only named pipe.
//!
//! Start: LockFileEx(host.lock, FAIL_IMMEDIATELY) -> package check -> create pipe
//! (DACL "D:P(A;;GA;;;<user SID>)", reject remote, FIRST_PIPE_INSTANCE) -> serve.
//! Each connection: one handshake line, then rmcp over the same stream (async-rw).
#![windows_subsystem = "windows"]

#[path = "../../bridge/src/common.rs"]
mod common;
mod winutil;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{Implementation, ProgressNotificationParam, RequestMetaObject, ServerCapabilities, ServerConfig};
use rmcp::{Peer, RoleServer, ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId;
use windows_sys::Win32::System::Threading::GetCurrentProcess;

fn dlog(msg: &str) {
    common::log_line("daemon.log", msg);
}

static MARKER: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// Every exit path removes this daemon's marker file.
fn exit_clean(code: i32) -> ! {
    if let Some(m) = MARKER.get() {
        let _ = std::fs::remove_file(m);
    }
    std::process::exit(code)
}

struct State {
    launched_by: String,
    in_job: Option<bool>,
    package: Option<String>,
    sessions: AtomicUsize,
    total_sessions: AtomicUsize,
    last_activity_ms: AtomicU64,
    pipe_sddl: std::sync::Mutex<String>,
}

#[derive(Clone)]
struct Daemon {
    st: Arc<State>,
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ProgressReq {
    /// Number of progress steps to emit.
    steps: Option<u32>,
}

#[tool_router]
impl Daemon {
    #[tool(description = "Liveness check; reports daemon identity and launch context")]
    fn ping(&self) -> String {
        let ppid = winutil::parent_of(std::process::id());
        serde_json::json!({
            "pong": true,
            "daemon_pid": std::process::id(),
            "parent": ppid.map(|p| format!("{} {}", p.pid, p.name)),
            "launched_by": self.st.launched_by,
            "in_job": self.st.in_job,
            "package": self.st.package,
            "data_dir": common::data_dir(),
            "sessions": self.st.sessions.load(Ordering::SeqCst),
            "total_sessions": self.st.total_sessions.load(Ordering::SeqCst),
        })
        .to_string()
    }

    #[tool(description = "Emits `steps` progress notifications (needs _meta.progressToken)")]
    async fn progress(
        &self,
        Parameters(req): Parameters<ProgressReq>,
        meta: RequestMetaObject,
        client: Peer<RoleServer>,
    ) -> String {
        let steps = req.steps.unwrap_or(5);
        let Some(tok) = meta.get_progress_token() else {
            return "no progressToken supplied; 0 notifications".into();
        };
        for i in 1..=steps {
            let _ = client
                .notify_progress(
                    ProgressNotificationParam::new(tok.clone(), i as f64)
                        .with_total(steps as f64)
                        .with_message(format!("step {i}/{steps}")),
                )
                .await;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        format!("done {steps}")
    }

    #[tool(description = "Effective SDDL of the daemon's pipe (GetSecurityInfo)")]
    fn pipe_sddl(&self) -> String {
        self.st.pipe_sddl.lock().unwrap().clone()
    }

    #[tool(description = "Stops the daemon (spike cleanup)")]
    fn shutdown(&self) -> String {
        dlog("shutdown tool called");
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(150));
            exit_clean(0);
        });
        "bye".into()
    }
}

#[tool_handler]
impl ServerHandler for Daemon {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("sr-daemon-spike", "0.0.1"))
            .with_instructions("M0-D spike daemon")
    }
}

fn arg_value(args: &[String], k: &str) -> Option<String> {
    args.iter().position(|a| a == k).and_then(|i| args.get(i + 1).cloned())
}

fn touch(st: &State) {
    st.last_activity_ms.store(common::now_ms() as u64, Ordering::SeqCst);
}

async fn read_line(pipe: &mut NamedPipeServer) -> std::io::Result<String> {
    let mut v = Vec::new();
    let mut b = [0u8; 1];
    loop {
        if pipe.read(&mut b).await? == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        if b[0] == b'\n' {
            return Ok(String::from_utf8_lossy(&v).into_owned());
        }
        v.push(b[0]);
        if v.len() > 4096 {
            return Err(std::io::Error::other("handshake too long"));
        }
    }
}

async fn session(mut pipe: NamedPipeServer, d: Daemon) {
    let st = d.st.clone();
    let mut cpid = 0u32;
    unsafe { GetNamedPipeClientProcessId(std::os::windows::io::AsRawHandle::as_raw_handle(&pipe) as HANDLE, &mut cpid) };
    let hs = match tokio::time::timeout(Duration::from_secs(5), read_line(&mut pipe)).await {
        Ok(Ok(l)) => l,
        other => {
            dlog(&format!("handshake failed from pid {cpid}: {other:?}"));
            return;
        }
    };
    // Handshake {proto, build, client_kind, client_id}. Accept MIN_PROTO..=PROTO (N-1..N).
    let v: serde_json::Value = serde_json::from_str(&hs).unwrap_or_default();
    let proto = v.get("proto").and_then(|x| x.as_u64()).unwrap_or(0);
    let kind = v.get("client_kind").and_then(|x| x.as_str()).unwrap_or("");
    let refuse = if proto < common::MIN_PROTO as u64 {
        Some(("Unavailable", "upgrade_client", common::MSG_CLIENT_OUTDATED))
    } else if proto > common::PROTO as u64 {
        Some(("Unavailable", "upgrade_daemon", common::MSG_DAEMON_OUTDATED))
    } else if kind != "mcp" && kind != "app" {
        Some(("InvalidRequest", "bad_client_kind", "Unknown client type."))
    } else {
        None
    };
    if let Some((code, reason, msg)) = refuse {
        let r = serde_json::json!({"ok": false, "code": code, "reason": reason, "message": msg,
            "proto": common::PROTO, "min_proto": common::MIN_PROTO, "build": common::BUILD});
        let _ = pipe.write_all(format!("{r}\n").as_bytes()).await;
        let _ = pipe.flush().await;
        dlog(&format!("refused handshake ({reason}) proto={proto} kind={kind} from pid {cpid}: {hs}"));
        return;
    }
    let reply = format!(
        "{{\"ok\":true,\"proto\":{},\"build\":\"{}\",\"daemon_pid\":{}}}\n",
        common::PROTO,
        common::BUILD,
        std::process::id()
    );
    if pipe.write_all(reply.as_bytes()).await.is_err() {
        return;
    }
    st.sessions.fetch_add(1, Ordering::SeqCst);
    st.total_sessions.fetch_add(1, Ordering::SeqCst);
    touch(&st);
    dlog(&format!("session open client_pid={cpid} hs={hs}"));
    match d.clone().serve(pipe).await {
        Ok(running) => {
            let r = running.waiting().await;
            dlog(&format!("session end client_pid={cpid}: {r:?}"));
        }
        Err(e) => dlog(&format!("session init error client_pid={cpid}: {e}")),
    }
    st.sessions.fetch_sub(1, Ordering::SeqCst);
    touch(&st);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // Trampoline (double spawn): re-launch self with breakaway and exit at once, so the real
    // daemon's parent PID is dead and `taskkill /T` on the bridge cannot reach it.
    if args.iter().any(|a| a == "--trampoline") {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x8;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        let r = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--launched-by", "trampoline"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB)
            .spawn();
        dlog(&format!("trampoline spawned: {:?}", r.as_ref().map(|c| c.id())));
        std::process::exit(if r.is_ok() { 0 } else { 3 });
    }
    let launched_by =arg_value(&args, "--launched-by").unwrap_or_else(|| "direct".into());
    let idle_secs: u64 = arg_value(&args, "--idle-exit-secs").and_then(|s| s.parse().ok()).unwrap_or(30);
    let skip_host_lock = args.iter().any(|a| a == "--skip-host-lock");
    let in_job = winutil::in_any_job(unsafe { GetCurrentProcess() });
    let package = winutil::package_full_name();
    let parent = winutil::parent_of(std::process::id());
    dlog(&format!(
        "start launched_by={launched_by} in_job={in_job:?} own_job_flags={} package={package:?} parent={parent:?} exe={:?}",
        winutil::job_limit_flags(std::ptr::null_mut()).map(winutil::job_flag_names).unwrap_or_else(|| "-".into()),
        std::env::current_exe().ok()
    ));

    // 1. Single host per user+data dir.
    let _host_lock = if skip_host_lock {
        dlog("host.lock skipped (--skip-host-lock test mode)");
        None
    } else {
        match common::lock_file("host.lock", false) {
            Ok(f) => Some(f),
            Err(e) => {
                dlog(&format!("host.lock held by another daemon ({e}); exiting cleanly"));
                std::process::exit(0);
            }
        }
    };
    // 2. Refuse to own stores in a packaged (MSIX) context.
    if package.is_some() && !args.iter().any(|a| a == "--allow-packaged") {
        dlog("packaged context detected; refusing to own stores");
        std::process::exit(4);
    }
    // 3. Marker file proves where writes land (virtualisation check).
    let marker = common::data_dir().join(format!("daemon-{}.json", std::process::id()));
    let _ = MARKER.set(marker.clone());
    let _ = std::fs::write(
        &marker,
        serde_json::json!({"pid": std::process::id(), "launched_by": launched_by, "in_job": in_job, "package": package}).to_string(),
    );

    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let code = rt.block_on(async move {
        let name = common::pipe_name();
        let sid = common::user_sid_string().expect("sid");
        // SR_PIPE_LABEL=1 (spike variant): explicit Medium label with no-write-up + no-read-up.
        let label = if std::env::var_os("SR_PIPE_LABEL").is_some() { "S:(ML;;NWNR;;;ME)" } else { "" };
        let sddl = format!("D:P(A;;GA;;;{sid}){label}");
        let sd = winutil::sd_from_sddl(&sddl).expect("sddl");
        let mut sa = winutil::security_attributes(sd);
        let create = |first: bool, sa: &mut windows_sys::Win32::Security::SECURITY_ATTRIBUTES| unsafe {
            ServerOptions::new()
                .first_pipe_instance(first)
                .reject_remote_clients(true)
                .max_instances(254)
                .create_with_security_attributes_raw(&name, (sa as *mut windows_sys::Win32::Security::SECURITY_ATTRIBUTES).cast())
        };
        let mut server = match create(true, &mut sa) {
            Ok(s) => s,
            Err(e) => {
                dlog(&format!("pipe create failed (FIRST_PIPE_INSTANCE) {name}: {e}; exiting cleanly"));
                return 0;
            }
        };
        let eff = winutil::handle_sddl(std::os::windows::io::AsRawHandle::as_raw_handle(&server) as HANDLE)
            .unwrap_or_else(|e| format!("<GetSecurityInfo error {e}>"));
        dlog(&format!("pipe ready {name} requested={sddl} effective={eff}"));
        let st = Arc::new(State {
            launched_by,
            in_job,
            package,
            sessions: AtomicUsize::new(0),
            total_sessions: AtomicUsize::new(0),
            last_activity_ms: AtomicU64::new(common::now_ms() as u64),
            pipe_sddl: std::sync::Mutex::new(format!("requested={sddl} effective={eff}")),
        });
        let d = Daemon { st: st.clone(), tool_router: Daemon::tool_router() };
        // Idle exit: no sessions for idle_secs.
        let st2 = st.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let idle = common::now_ms() as u64 - st2.last_activity_ms.load(Ordering::SeqCst);
                if st2.sessions.load(Ordering::SeqCst) == 0 && idle > idle_secs * 1000 {
                    dlog(&format!("idle {idle} ms with no sessions; exiting"));
                    exit_clean(0);
                }
            }
        });
        loop {
            if let Err(e) = server.connect().await {
                dlog(&format!("connect error: {e}"));
                continue;
            }
            let connected = server;
            server = match create(false, &mut sa) {
                Ok(s) => s,
                Err(e) => {
                    dlog(&format!("next instance create failed: {e}"));
                    return 2;
                }
            };
            tokio::spawn(session(connected, d.clone()));
        }
    });
    exit_clean(code);
}
