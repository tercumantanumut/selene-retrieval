//! The `sr-daemon` process (docs/02 §2.2, M1-C contract, Amendments 3.12, 3.13, 4.2, 4.8).
//!
//! Start order (the binary's `main` does steps 1–2 before anything else):
//! 1. `sr_providers::dml::harden_dll_search()` — first statement of `main` (4.8);
//! 2. install the single rustls crypto provider (3.13);
//! 3. data dir: short plain local path (3.12), created, protected ACL (user + SYSTEM);
//! 4. `HostLock::acquire` (another daemon holds it → exit 0 quietly);
//! 5. `packaged_context_error()` → refuse to own stores in an MSIX context (exit 4, U8);
//! 6. `pipe::serve` on an [`EngineSlot`] (M2 tester O1): the handshake, MCP `initialize`,
//!    `tools/list`, status ("Starting…") and event subscriptions answer while the engine starts;
//!    calls that need the engine wait for it briefly;
//! 7. M4-E (page 32): a saved index move runs here, before any store is opened: copy, verify,
//!    atomic pointer switch, with `index.move.*` events on the pipe's bus;
//! 8. `Engine::start` (catalog → epoch → model + GPU detection → stores → watermark copy →
//!    finalize_applied → roots → tier detection) publishing on the pipe's bus; then the slot is
//!    ready. If the start fails right after a move, the pointer goes back to the old copy and the
//!    start is tried once more there. A failed start drains the pipe and exits 3. After a
//!    successful start the old copy of a moved index is deleted (background);
//! 9. M4-E: the HTTP listener follows the engine's policy (off by default); an engine restart
//!    request (index move) drains the pipe. Idle rule (never while HTTP is on); `stopped()` →
//!    exit 0.
//!
//! R34 (M5-E): when the moved search index is missing (step 3), the daemon does not exit 3 at
//! once. It opens the pipe in a **recovery mode** ([`run_recovery`]): status "Search index not
//! found" with the details, every engine call answers `IndexMissing` ("Reconnect the drive, or
//! move the index back"), `index_location.recover` (Move it back / Start fresh) is served, and
//! the daemon restarts by itself as soon as the folder is back. Idle → exit 3 as before.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use sr_engine::pipe::{self, HostLock, PipeServerConfig};
use sr_engine::{Engine, EngineConfig, EventBus};

use crate::service::DaemonService;
use crate::slot::EngineSlot;

/// Exit codes.
pub const EXIT_OK: u8 = 0;
pub const EXIT_USAGE: u8 = 2;
pub const EXIT_START_FAILED: u8 = 3;
pub const EXIT_PACKAGED: u8 = 4;

/// Idle rule (U24): exit after this long with no session and an empty queue.
pub const IDLE_EXIT: Duration = Duration::from_secs(10 * 60);

/// R34: how often the recovery mode checks whether the missing index is back.
pub const RECOVERY_POLL: Duration = Duration::from_secs(3);

/// Parsed command line.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DaemonArgs {
    pub data_dir: Option<PathBuf>,
    pub launched_by: String,
    /// Debug builds only: where `models\` and `sr-extract.exe` live (default: the exe's dir).
    pub app_dir: Option<PathBuf>,
    pub version: bool,
}

/// `sr-daemon [--data-dir <dir>] [--launched-by <how>] [--version]`.
pub fn parse_args(args: &[String]) -> Result<DaemonArgs, String> {
    let mut a = DaemonArgs {
        launched_by: "direct".into(),
        ..DaemonArgs::default()
    };
    let mut it = args.iter();
    while let Some(x) = it.next() {
        match x.as_str() {
            "--data-dir" => {
                a.data_dir = Some(PathBuf::from(it.next().ok_or("--data-dir needs a value")?))
            }
            "--launched-by" => {
                a.launched_by = it.next().ok_or("--launched-by needs a value")?.clone()
            }
            "--app-dir" if cfg!(debug_assertions) => {
                a.app_dir = Some(PathBuf::from(it.next().ok_or("--app-dir needs a value")?));
            }
            "--version" | "-V" => a.version = true,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(a)
}

/// Install directory: the exe's directory (debug builds: `--app-dir` or `SR_APP_DIR` override).
pub fn app_dir(explicit: Option<PathBuf>) -> PathBuf {
    if cfg!(debug_assertions) {
        if let Some(p) = explicit {
            return p;
        }
        if let Some(p) = sr_client::naming::dev_env("SR_APP_DIR") {
            return PathBuf::from(p);
        }
    }
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn init_logging(dir: &std::path::Path) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::EnvFilter;
    let logs = dir.join("logs");
    std::fs::create_dir_all(&logs).ok()?;
    let appender = tracing_appender::rolling::daily(&logs, "sr-daemon.log");
    let (w, guard) = tracing_appender::non_blocking(appender);
    // Env override only in debug builds (U3). INFO never carries query or file text.
    let filter = sr_client::naming::dev_env("SR_LOG")
        .and_then(|v| EnvFilter::try_new(v.to_string_lossy()).ok())
        .unwrap_or_else(|| EnvFilter::new("info,lance=warn,lancedb=warn,tantivy=warn,ort=warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(w)
        .with_ansi(false)
        .try_init()
        .ok()?;
    Some(guard)
}

/// Run the daemon; returns the process exit code.
pub fn run(args: DaemonArgs) -> u8 {
    if args.version {
        println!("sr-daemon {}", env!("CARGO_PKG_VERSION"));
        return EXIT_OK;
    }
    // 3. Data dir (never from the environment in release builds).
    let dir = match sr_client::naming::resolve_data_dir(args.data_dir.as_deref()) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{}", e.user_message().agent_text());
            return EXIT_START_FAILED;
        }
    };
    // R34 (M5-E): a moved index that is missing → recovery mode (never an empty fork).
    if let Some(info) = sr_engine::recover::missing_index(&dir) {
        return run_recovery(dir, info);
    }
    let layout = match sr_engine::datadir::prepare(&dir) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("{}", e.user_message().agent_text());
            return EXIT_START_FAILED;
        }
    };
    if let Err(e) = sr_engine::datadir::restrict_acl(&dir) {
        // Never weaken silently: refuse to run with a data dir we could not protect.
        eprintln!("{}", e.user_message().agent_text());
        return EXIT_START_FAILED;
    }
    let _log = init_logging(&dir);
    // 4. One host per user + data dir.
    let lock = match HostLock::acquire(&dir) {
        Ok(Some(l)) => l,
        Ok(None) => {
            tracing::info!(target: "sr_daemon", "another daemon owns this data folder; exiting");
            return EXIT_OK;
        }
        Err(e) => {
            tracing::error!(target: "sr_daemon", err = %e, "host lock failed");
            return EXIT_START_FAILED;
        }
    };
    // 5. Never own stores from a packaged (MSIX) context.
    if let Some(e) = pipe::packaged_context_error() {
        tracing::error!(target: "sr_daemon", err = %e, "packaged context; refusing to start");
        return EXIT_PACKAGED;
    }
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("sr-daemon")
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!(target: "sr_daemon", err = %e, "runtime");
            return EXIT_START_FAILED;
        }
    };
    let app = app_dir(args.app_dir.clone());
    let code = rt.block_on(async move {
        // 6. Pipe first (O1): the model load and GPU detection (1-2 s) no longer delay the
        //    handshake. Events published during the start reach early subscribers in order.
        let bus = Arc::new(EventBus::default());
        let (slot, setter) = EngineSlot::starting(bus.clone());
        // `tools/list` before the engine is ready lists `add_folder` and `sync_folder` per the
        // settings file (read only; the engine creates it with defaults on a first start).
        // M5-E: the machine policy wins here too (a locked tool is never listed).
        let early_policy = tokio::task::spawn_blocking(sr_engine::policy::load_machine)
            .await
            .unwrap_or_default();
        let file = std::fs::read_to_string(&layout.settings)
            .map(|t| sr_common::config::from_toml_str(&t).0)
            .ok()
            .map(|mut s| {
                early_policy.apply(&mut s);
                s
            });
        let add_folder = file
            .as_ref()
            .is_some_and(|s| s.integrations.mcp_add_folder_enabled);
        let sync_folder = file
            .as_ref()
            .is_some_and(|s| s.integrations.mcp_sync_folder_enabled);
        // U17: `context_answer` is listed at once when a provider is configured and answers are
        // on; if it then doesn't connect, sessions get `tools/list_changed` once the engine is
        // ready.
        let provider = file
            .as_ref()
            .is_some_and(|s| s.provider.provider.as_deref().is_some_and(|p| !p.is_empty()));
        let answers = provider && file.as_ref().is_some_and(|s| s.answers.answers_enabled);
        // M4-E (page 32): a saved index move runs before the engine opens the stores.
        let move_ctl = Arc::new(sr_engine::indexmove::MoveControl::default());
        let pending_move = sr_engine::indexmove::read_plan(&dir);
        if let Some(p) = &pending_move {
            move_ctl.set_waiting(p);
        }
        let slot = slot
            .with_add_folder_hint(add_folder)
            .with_sync_folder_hint(sync_folder)
            .with_provider_hint(answers)
            .with_move_control(move_ctl.clone());
        let http_slot = slot.clone();
        let svc = Arc::new(DaemonService::with_slot(slot));
        let server = match pipe::serve(PipeServerConfig::new(dir.clone()), svc) {
            Ok(h) => h,
            Err(e) => {
                tracing::error!(target: "sr_daemon", err = %e, "pipe did not open");
                return EXIT_OK; // another daemon won the pipe name (FIRST_PIPE_INSTANCE)
            }
        };
        tracing::info!(target: "sr_daemon", pipe = server.pipe_name(), "pipe open; engine starting");
        // 7. Engine (catalog → epoch → model → stores → watermark copy → finalize_applied → roots
        //    → tier detection). All cheap after the model load: outbox work left by the last run
        //    is applied by the background writer ("Catching up after restart", Amendment 4.24).
        let moved = if pending_move.is_some() {
            let (d, ctl, b) = (dir.clone(), move_ctl.clone(), bus.clone());
            tokio::task::spawn_blocking(move || {
                sr_engine::indexmove::run_pending(&d, &ctl, &|e| {
                    b.publish(e);
                })
            })
            .await
            .unwrap_or(sr_engine::indexmove::MoveOutcome::Nothing)
        } else {
            sr_engine::indexmove::MoveOutcome::Nothing
        };
        let app_exe = app.join(APP_EXE);
        let mut cfg = EngineConfig::new(dir.clone(), app);
        cfg.launched_by = args.launched_by.clone();
        cfg.bus = Some(bus.clone());
        let engine = match Engine::start(cfg.clone()).await {
            Ok(e) => Ok(e),
            Err(e) if matches!(moved, sr_engine::indexmove::MoveOutcome::Switched { .. }) => {
                // The new location didn't start: point back to the old copy (still intact) and
                // try once more there. The user sees "Nothing was moved".
                tracing::error!(target: "sr_daemon", code = %e.code, err = %e, "engine did not start on the moved index; going back");
                let d = dir.clone();
                let back = tokio::task::spawn_blocking(move || sr_engine::indexmove::revert_switch(&d))
                    .await
                    .ok()
                    .flatten();
                match back {
                    Some(old) => {
                        bus.publish(sr_common::events::EngineEvent::IndexMoveFailed {
                            code: e.code,
                            kept_path: old.display().to_string(),
                        });
                        Engine::start(cfg).await
                    }
                    None => Err(e),
                }
            }
            Err(e) => Err(e),
        };
        let engine = match engine {
            Ok(e) => e,
            Err(e) => {
                tracing::error!(target: "sr_daemon", code = %e.code, err = %e, "engine did not start");
                setter.failed(e.code);
                server.begin_drain();
                server.stopped().await;
                return EXIT_START_FAILED;
            }
        };
        setter.ready(engine.clone());
        // Sessions computed their tool list from the settings hints until now; let them re-check
        // against the engine's own flags (`tools/list_changed` if they differ).
        engine
            .bus()
            .publish(sr_common::events::EngineEvent::SettingsChanged);
        tracing::info!(target: "sr_daemon", pipe = server.pipe_name(), launched_by = %args.launched_by, "ready");
        // R-UX30: an `add_folder` request while the app is closed opens the app on it.
        let approvals = spawn_approval_launcher(server.clone(), engine.bus_arc(), app_exe);
        // M4-E (page 32): delete the old copy of a moved index now that the new one runs.
        {
            let (d, e2) = (dir.clone(), engine.clone());
            tokio::spawn(async move {
                let root = e2.data_dir().to_path_buf();
                let done = tokio::task::spawn_blocking(move || {
                    sr_engine::indexmove::cleanup_after_start(&d, &root)
                })
                .await
                .ok()
                .flatten();
                if let Some(plan) = done {
                    e2.note_index_moved(&plan.to).await;
                }
            });
        }
        // M4-E: the opt-in HTTP listener (off by default) and the engine's restart requests.
        let http = crate::http::supervise(engine.clone(), http_slot);
        let restart = {
            let (server, engine) = (server.clone(), engine.clone());
            tokio::spawn(async move {
                engine.restart_requested().await;
                tracing::info!(target: "sr_daemon", "restart requested; draining");
                server.begin_drain();
            })
        };
        let idle = {
            let (server, engine) = (server.clone(), engine.clone());
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    if server.is_draining() {
                        break;
                    }
                    // "Empty queue" includes the writer's backlog (outbox ops still to embed and
                    // commit) and jobs in flight, not just queued jobs: on a CPU-only machine one
                    // group commit can take many minutes, and exiting mid-batch threw that work
                    // away on every idle cycle (M1 tester fix).
                    // M4-E: never while HTTP is on (local tools can't launch the daemon).
                    if server.sessions() == 0
                        && server.idle_for() >= IDLE_EXIT
                        && !engine.http_status().enabled
                        && engine.wait_idle(Duration::ZERO).await
                    {
                        tracing::info!(target: "sr_daemon", "idle; exiting");
                        server.begin_drain();
                        break;
                    }
                }
            })
        };
        server.stopped().await;
        idle.abort();
        approvals.abort();
        restart.abort();
        http.abort();
        EXIT_OK
    });
    drop(lock);
    code
}

/// R34 (M5-E): serve the pipe while the moved index is missing. Returns the exit code: 0 after
/// a recovery choice or when the index came back (the next attach starts normally), 3 after the
/// idle time with no client.
pub fn run_recovery(dir: PathBuf, info: sr_common::api::IndexMissingInfo) -> u8 {
    let e = sr_engine::recover::missing_error(&info);
    eprintln!("{}", e.user_message().agent_text());
    if let Err(e) = sr_engine::datadir::restrict_acl(&dir) {
        eprintln!("{}", e.user_message().agent_text());
        return EXIT_START_FAILED;
    }
    let _log = init_logging(&dir);
    let lock = match HostLock::acquire(&dir) {
        Ok(Some(l)) => l,
        Ok(None) => return EXIT_OK,
        Err(e) => {
            tracing::error!(target: "sr_daemon", err = %e, "host lock failed");
            return EXIT_START_FAILED;
        }
    };
    if let Some(e) = pipe::packaged_context_error() {
        tracing::error!(target: "sr_daemon", err = %e, "packaged context; refusing to start");
        return EXIT_PACKAGED;
    }
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("sr-daemon")
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!(target: "sr_daemon", err = %e, "runtime");
            return EXIT_START_FAILED;
        }
    };
    tracing::warn!(target: "sr_daemon", old_copy = info.old_copy.is_some(), folders = info.folders_to_restore, "the moved search index is missing; waiting in recovery mode");
    let code = rt.block_on(async move {
        // The machine policy also applies here (locked fields show in the status).
        let policy = tokio::task::spawn_blocking(sr_engine::policy::load_machine)
            .await
            .unwrap_or_default();
        sr_engine::policy::log(&policy);
        sr_common::config::policy::install(policy);
        let bus = Arc::new(EventBus::default());
        let (slot, setter) = EngineSlot::starting(bus);
        let slot = slot.with_recovery(dir.clone());
        setter.index_missing(info);
        let svc = Arc::new(DaemonService::with_slot(slot.clone()));
        let server = match pipe::serve(PipeServerConfig::new(dir.clone()), svc) {
            Ok(h) => h,
            Err(e) => {
                tracing::error!(target: "sr_daemon", err = %e, "pipe did not open");
                return EXIT_OK;
            }
        };
        let mut tick = tokio::time::interval(RECOVERY_POLL);
        let code = loop {
            tokio::select! {
                () = slot.restart_requested() => break EXIT_OK,
                _ = tick.tick() => {
                    let d = dir.clone();
                    let back = tokio::task::spawn_blocking(move || {
                        sr_engine::recover::missing_index(&d).is_none()
                    })
                    .await
                    .unwrap_or(false);
                    if back {
                        tracing::info!(target: "sr_daemon", "the search index is reachable again; restarting");
                        break EXIT_OK;
                    }
                    if server.sessions() == 0 && server.idle_for() >= IDLE_EXIT {
                        tracing::info!(target: "sr_daemon", "idle in recovery mode; exiting");
                        break EXIT_START_FAILED;
                    }
                }
            }
        };
        server.begin_drain();
        server.stopped().await;
        code
    });
    drop(lock);
    code
}

/// The desktop app next to the daemon (install layout).
pub const APP_EXE: &str = "sr-app.exe";

/// R-UX30 (Q5 / R-UX12): when an `add_folder` approval is requested and no app session is
/// attached, start `sr-app.exe --approve <id>` (absolute path next to the daemon; once per
/// request). A running app handles it itself (it is attached, or its single-instance handler
/// takes the id). Debug builds skip the launch with `SR_NO_APP_LAUNCH` (tests).
pub fn spawn_approval_launcher(
    server: sr_engine::pipe::ServerHandle,
    bus: Arc<EventBus>,
    app_exe: std::path::PathBuf,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut rx = bus.subscribe();
        let mut launched = std::collections::HashSet::new();
        loop {
            let ev = match rx.recv().await {
                Ok(ev) => ev,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            };
            if let sr_common::events::EngineEvent::ApprovalRequested { approval_id, .. } =
                ev.payload
                && launched.insert(approval_id)
            {
                // Give an app that is just attaching a moment (off the event loop, so a burst of
                // events can't make this receiver lag).
                let (server, app_exe) = (server.clone(), app_exe.clone());
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    if server.app_sessions() == 0 {
                        launch_app_for_approval(&app_exe, approval_id);
                    }
                });
            }
        }
    })
}

fn launch_app_for_approval(app_exe: &std::path::Path, id: sr_common::ids::ApprovalId) {
    if sr_client::naming::dev_env("SR_NO_APP_LAUNCH").is_some() {
        tracing::info!(target: "sr_daemon", "approval requested; app launch skipped (SR_NO_APP_LAUNCH)");
        return;
    }
    if !app_exe.is_file() {
        tracing::warn!(target: "sr_daemon", "approval requested but the app is not installed next to the service");
        return;
    }
    let mut cmd = std::process::Command::new(app_exe);
    cmd.arg("--approve").arg(id.to_string());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // No console, own process group: the app outlives nothing of ours.
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    match cmd.spawn() {
        Ok(_) => {
            tracing::info!(target: "sr_daemon", "approval requested; opened the app to show it")
        }
        Err(e) => {
            tracing::warn!(target: "sr_daemon", err = %e, "could not open the app for an approval")
        }
    }
}
