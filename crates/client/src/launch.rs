//! `sr-launch`: the short-lived launcher between a client and the daemon (Amendment 3.9, M0
//! tester item O1).
//!
//! Order (not packaged):
//! 1. **Verified breakaway.** `CreateProcess(DETACHED | NEW_PROCESS_GROUP | BREAKAWAY_FROM_JOB |
//!    SUSPENDED, inherit=FALSE)`, then `IsProcessInJob(child, NULL)` *before the child runs a
//!    single instruction*. A nested Job whose outer job forbids breakaway lets `CreateProcess`
//!    succeed while the child stays in the outer job (M0-D 3f); such a child is terminated and we
//!    fall through. Otherwise it is resumed and the launcher exits, so the daemon's parent is
//!    dead and `taskkill /T` on the client tree can't reach it (M0-D 3h).
//! 2. **Per-user scheduled task** (`schtasks /run`; self-registered from XML if missing or
//!    pointing elsewhere — no admin needed). Task Scheduler's job never kills on close.
//! 3. **In-job** (degraded, last resort): the daemon lives as long as the client's job.
//!
//! Packaged (MSIX) context: breakaway does not drop package identity, so only the task is used
//! (in-job would put the daemon's writes in the package's virtualised folder).
//!
//! Unix (macOS port): there are no Job Objects. "Breakaway" is a detached start in a new session
//! (`setsid`, stdio on `/dev/null`, `sys::spawn_detached`); the launcher exits at once, so the
//! daemon's parent is launchd. The task and in-job methods report "not available".
//!
//! No LaunchAgent (macOS phase 2 decision). On Windows the scheduled task exists to escape a
//! kill-on-close Job Object (MSIX, MCP clients that kill their process tree). macOS has neither:
//! a process in its own session and process group, re-parented to launchd, is out of reach of
//! the client's group signals and of its exit, and nothing kills it with the client. A
//! LaunchAgent would add a plist in `~/Library/LaunchAgents`, a "Background item added" notice
//! and (with `SMAppService`) a signed bundle, for no gain in lifetime. A sandboxed MCP client
//! would not be helped either: its children inherit the sandbox, and so would `launchctl`.
//! Open for phase 4 (packaging): privacy prompts (TCC) name the app that started the daemon.

use std::io::Write;
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::process::{Command, Stdio};
#[cfg(windows)]
use std::time::Duration;

#[cfg(windows)]
use windows_sys::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, DETACHED_PROCESS,
};

use crate::naming::{self, LAUNCH_LOG};
use crate::sys;
#[cfg(windows)]
use crate::win::{self, OwnedProcess};

/// `sr-launch` exit codes.
pub mod code {
    /// Daemon started outside every Job Object (verified).
    pub const BREAKAWAY: u32 = 0;
    /// Daemon started by the scheduled task.
    pub const TASK: u32 = 10;
    /// Daemon started inside the client's job (degraded; lives as long as the client).
    pub const IN_JOB: u32 = 11;
    /// Bad arguments.
    pub const USAGE: u32 = 2;
    /// Packaged context and the task could not be used ("Open Selene Retrieval once to finish
    /// setup.", U8).
    pub const PACKAGED_NO_TASK: u32 = 20;
    /// Every method failed.
    pub const FAILED: u32 = 21;

    /// Whether the launcher started a daemon process.
    pub const fn is_started(c: u32) -> bool {
        matches!(c, BREAKAWAY | TASK | IN_JOB)
    }
}

/// A launch method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Breakaway,
    Task,
    InJob,
}

impl Method {
    fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "breakaway" => Some(Method::Breakaway),
            "task" => Some(Method::Task),
            "injob" => Some(Method::InJob),
            _ => None,
        }
    }
}

/// Launcher inputs.
#[derive(Debug, Clone)]
pub struct LaunchArgs {
    pub data_dir: PathBuf,
    pub daemon_exe: PathBuf,
    pub task_name: String,
    pub order: Vec<Method>,
    pub packaged: bool,
}

impl LaunchArgs {
    /// Defaults: `sr-daemon.exe` next to `sr-launch.exe`, the per-user task, order
    /// breakaway → task → in-job (task only when packaged; Unix: breakaway only). Debug builds
    /// honour `SR_DAEMON_EXE`, `SR_TASK_PREFIX` and `SR_LAUNCH_ORDER`.
    pub fn new(data_dir: PathBuf) -> Result<Self, String> {
        naming::validate_data_dir(&data_dir).map_err(|e| e.to_string())?;
        let daemon_exe = match naming::dev_env("SR_DAEMON_EXE") {
            Some(p) => PathBuf::from(p),
            None => naming::sibling_exe(&naming::exe_file_name("sr-daemon"))
                .map_err(|e| e.to_string())?,
        };
        let sid = sys::user_sid_string().map_err(|e| e.to_string())?;
        let task_name = naming::task_name(&sid, &data_dir);
        let packaged = sys::package_full_name().is_some();
        let order = match naming::dev_env("SR_LAUNCH_ORDER") {
            Some(o) => o
                .to_string_lossy()
                .split(',')
                .filter_map(Method::parse)
                .collect(),
            None if cfg!(windows) => vec![Method::Breakaway, Method::Task, Method::InJob],
            None => vec![Method::Breakaway],
        };
        Ok(Self {
            data_dir,
            daemon_exe,
            task_name,
            order,
            packaged,
        })
    }

    #[cfg(windows)]
    fn daemon_args(&self, launched_by: &str) -> String {
        format!(
            "--data-dir {} --launched-by {launched_by}",
            win::quote_arg(&self.data_dir.to_string_lossy())
        )
    }
}

/// What happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Breakaway { pid: u32 },
    Task { registered: bool },
    InJob { pid: u32 },
    PackagedNoTask(String),
    Failed(String),
}

impl Outcome {
    pub fn exit_code(&self) -> u32 {
        match self {
            Outcome::Breakaway { .. } => code::BREAKAWAY,
            Outcome::Task { .. } => code::TASK,
            Outcome::InJob { .. } => code::IN_JOB,
            Outcome::PackagedNoTask(_) => code::PACKAGED_NO_TASK,
            Outcome::Failed(_) => code::FAILED,
        }
    }
}

/// Appends one line to `<data_dir>\launch.log` (best effort; capped at 256 KiB).
pub fn log(data_dir: &Path, line: &str) {
    let p = data_dir.join(LAUNCH_LOG);
    if std::fs::metadata(&p).is_ok_and(|m| m.len() > 256 * 1024) {
        let _ = std::fs::remove_file(&p);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&p)
    {
        let ms = sr_common::events::now_ms();
        let _ = f.write_all(format!("{ms} pid={} {line}\n", std::process::id()).as_bytes());
    }
}

/// Runs the launcher. Never blocks longer than the task registration (~1.5 s).
pub fn run(args: &LaunchArgs) -> Outcome {
    let dd = &args.data_dir;
    let _ = std::fs::create_dir_all(dd);
    log(
        dd,
        &format!(
            "launch start order={:?} packaged={} launcher_in_job={:?} daemon={}",
            args.order,
            args.packaged,
            sys::current_process_in_job(),
            args.daemon_exe.display()
        ),
    );
    let mut errors = Vec::new();
    for m in &args.order {
        let r = match m {
            Method::Breakaway if args.packaged => Err("skipped: packaged context".into()),
            Method::Breakaway => breakaway(args),
            Method::Task => task(args),
            Method::InJob if args.packaged => Err("skipped: packaged context".into()),
            Method::InJob => in_job(args),
        };
        match r {
            Ok(o) => {
                log(dd, &format!("launch ok {o:?}"));
                return o;
            }
            Err(e) => {
                log(dd, &format!("launch {m:?} failed: {e}"));
                errors.push(format!("{m:?}: {e}"));
            }
        }
    }
    let msg = errors.join("; ");
    if args.packaged {
        Outcome::PackagedNoTask(msg)
    } else {
        Outcome::Failed(msg)
    }
}

#[cfg(windows)]
fn breakaway(args: &LaunchArgs) -> Result<Outcome, String> {
    let child = OwnedProcess::spawn(
        &args.daemon_exe,
        &args.daemon_args("breakaway"),
        DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB,
        true,
        args.daemon_exe.parent(),
    )
    .map_err(|e| format!("CreateProcess(BREAKAWAY_FROM_JOB): {e}"))?;
    // O1: verify before the daemon runs. Fail closed if the check itself fails.
    let jobbed = child.in_any_job().unwrap_or(true);
    if jobbed {
        let _ = child.terminate(1);
        let _ = child.wait(Duration::from_secs(2));
        return Err(format!(
            "child pid {} is still inside a Job Object (nested job forbids breakaway); killed",
            child.pid()
        ));
    }
    child.resume().map_err(|e| {
        let _ = child.terminate(1);
        format!("ResumeThread: {e}")
    })?;
    Ok(Outcome::Breakaway { pid: child.pid() })
}

/// Unix: detached in a new session, no inherited stdio (see the module docs).
#[cfg(unix)]
fn breakaway(args: &LaunchArgs) -> Result<Outcome, String> {
    let a: [&std::ffi::OsStr; 4] = [
        "--data-dir".as_ref(),
        args.data_dir.as_os_str(),
        "--launched-by".as_ref(),
        "breakaway".as_ref(),
    ];
    let pid = sys::spawn_detached(&args.daemon_exe, &a, args.daemon_exe.parent())
        .map_err(|e| format!("spawn (setsid): {e}"))?;
    Ok(Outcome::Breakaway { pid })
}

#[cfg(unix)]
fn task(_: &LaunchArgs) -> Result<Outcome, String> {
    Err("not available on this platform".into())
}

#[cfg(unix)]
fn in_job(_: &LaunchArgs) -> Result<Outcome, String> {
    Err("not available on this platform".into())
}

#[cfg(windows)]
fn in_job(args: &LaunchArgs) -> Result<Outcome, String> {
    let child = OwnedProcess::spawn(
        &args.daemon_exe,
        &args.daemon_args("injob"),
        DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP,
        false,
        args.daemon_exe.parent(),
    )
    .map_err(|e| format!("CreateProcess: {e}"))?;
    Ok(Outcome::InJob { pid: child.pid() })
}

#[cfg(windows)]
fn schtasks() -> Result<PathBuf, String> {
    win::system_dir()
        .map(|d| d.join("schtasks.exe"))
        .map_err(|e| format!("GetSystemDirectory: {e}"))
}

/// Runs `schtasks.exe` with no window; returns (exit code, stdout).
#[cfg(windows)]
fn run_schtasks(args: &[&str]) -> Result<(i32, String), String> {
    use std::os::windows::process::CommandExt;
    let out = Command::new(schtasks()?)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("schtasks: {e}"))?;
    let code = out.status.code().unwrap_or(-1);
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok((code, text))
}

#[cfg(windows)]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Task definition: on demand only (no trigger), never times out, ignores battery, one instance,
/// normal priority, the user's own non-elevated interactive token (M0-D §5.9, validated).
#[cfg(windows)]
pub fn task_xml(sid: &str, daemon_exe: &Path, arguments: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>Starts Selene Retrieval's background search service when an app needs it.</Description></RegistrationInfo>
  <Principals><Principal id="Author"><UserId>{sid}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings><StopOnIdleEnd>false</StopOnIdleEnd><RestartOnIdle>false</RestartOnIdle></IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>5</Priority>
  </Settings>
  <Actions Context="Author"><Exec><Command>{}</Command><Arguments>{}</Arguments></Exec></Actions>
</Task>
"#,
        xml_escape(&daemon_exe.to_string_lossy()),
        xml_escape(arguments)
    )
}

/// Registers (or replaces) the task from XML. No admin needed for a per-user, non-elevated task.
#[cfg(windows)]
pub fn register_task(task_name: &str, data_dir: &Path, daemon_exe: &Path) -> Result<(), String> {
    let sid = win::user_sid_string().map_err(|e| e.to_string())?;
    let args = format!(
        "--data-dir {} --launched-by task",
        win::quote_arg(&data_dir.to_string_lossy())
    );
    let xml = task_xml(&sid, daemon_exe, &args);
    let path = data_dir.join(format!("task-{}.xml", std::process::id()));
    let mut bytes = vec![0xFF, 0xFE];
    for u in xml.encode_utf16() {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    std::fs::write(&path, bytes).map_err(|e| format!("write task xml: {e}"))?;
    let p = path.to_string_lossy().into_owned();
    let r = run_schtasks(&["/create", "/tn", task_name, "/xml", &p, "/f"]);
    let _ = std::fs::remove_file(&path);
    match r? {
        (0, _) => Ok(()),
        (c, out) => Err(format!("schtasks /create exit {c}: {}", out.trim())),
    }
}

/// Deletes the task (uninstaller, tests).
#[cfg(windows)]
pub fn delete_task(task_name: &str) -> Result<(), String> {
    match run_schtasks(&["/delete", "/tn", task_name, "/f"])? {
        (0, _) => Ok(()),
        (c, out) => Err(format!("schtasks /delete exit {c}: {}", out.trim())),
    }
}

/// Whether the registered task already runs `daemon_exe` for `data_dir`.
#[cfg(windows)]
fn task_matches(task_name: &str, data_dir: &Path, daemon_exe: &Path) -> bool {
    let Ok((0, xml)) = run_schtasks(&["/query", "/tn", task_name, "/xml"]) else {
        return false;
    };
    let lower = xml.to_lowercase();
    let exe = xml_escape(&daemon_exe.to_string_lossy()).to_lowercase();
    let dd = xml_escape(&data_dir.to_string_lossy()).to_lowercase();
    lower.contains(&format!("<command>{exe}</command>")) && lower.contains(&dd)
}

#[cfg(windows)]
fn task(args: &LaunchArgs) -> Result<Outcome, String> {
    let mut registered = false;
    if !task_matches(&args.task_name, &args.data_dir, &args.daemon_exe) {
        register_task(&args.task_name, &args.data_dir, &args.daemon_exe)?;
        registered = true;
    }
    match run_schtasks(&["/run", "/tn", &args.task_name])? {
        (0, _) => Ok(Outcome::Task { registered }),
        (c, out) => Err(format!("schtasks /run exit {c}: {}", out.trim())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn xml_is_escaped_and_has_no_trigger() {
        let x = task_xml(
            "S-1-5-21-1",
            Path::new(r"C:\A&B\sr-daemon.exe"),
            r#"--data-dir "C:\x y" --launched-by task"#,
        );
        assert!(x.contains(r"<Command>C:\A&amp;B\sr-daemon.exe</Command>"));
        assert!(x.contains("&quot;C:\\x y&quot;"));
        assert!(!x.contains("<Triggers>"));
        assert!(x.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        assert!(x.contains("<RunLevel>LeastPrivilege</RunLevel>"));
    }

    #[test]
    fn exit_codes() {
        assert!(code::is_started(Outcome::Breakaway { pid: 1 }.exit_code()));
        assert!(code::is_started(
            Outcome::Task { registered: true }.exit_code()
        ));
        assert!(code::is_started(Outcome::InJob { pid: 1 }.exit_code()));
        assert!(!code::is_started(
            Outcome::Failed(String::new()).exit_code()
        ));
        assert!(!code::is_started(
            Outcome::PackagedNoTask(String::new()).exit_code()
        ));
        assert_eq!(Method::parse(" task"), Some(Method::Task));
        assert_eq!(Method::parse("x"), None);
    }
}
