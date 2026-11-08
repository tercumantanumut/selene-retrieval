//! Shared test support: per-test data dirs, the stub daemon, bridge processes speaking MCP over
//! stdio, Win32 process/job/token helpers, and cleanup of every daemon and scheduled task a test
//! creates (data dirs under `%TEMP%\sr-m1c`, tasks `\SR-M1C-test-*`).
#![allow(unsafe_code, dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::{Mutex, MutexGuard, Once};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
    TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, OpenThread, PROCESS_ALL_ACCESS, PROCESS_TERMINATE, ResumeThread,
    THREAD_SUSPEND_RESUME, TerminateProcess,
};

pub const BRIDGE: &str = env!("CARGO_BIN_EXE_sr-bridge");
pub const LAUNCH: &str = env!("CARGO_BIN_EXE_sr-launch");
pub const STUB: &str = env!("CARGO_BIN_EXE_sr-test-daemon");
pub const TASK_PREFIX: &str = "SR-M1C-test";

static INIT: Once = Once::new();
static SERIAL: Mutex<()> = Mutex::new(());
static COUNTER: AtomicU32 = AtomicU32::new(0);

/// Process-wide dev hooks (debug builds only; identical for every test, so set once).
pub fn init() {
    INIT.call_once(|| {
        // SAFETY: set once before any test spawns a child; Windows env access is locked by std.
        unsafe {
            std::env::set_var("SR_DAEMON_EXE", STUB);
            std::env::set_var("SR_LAUNCH_EXE", LAUNCH);
            std::env::set_var("SR_TASK_PREFIX", TASK_PREFIX);
            // The test exe is not one of our programs: S-05 (M5) would refuse its
            // `host.shutdown` and handshake drain-restart. These tests cover attach / events /
            // skew mechanics; the trust rules have their own tests in sr-engine (pipe, tests/m4.rs).
            std::env::set_var("SR_TRUST_ANY_APP", "1");
            std::env::remove_var("SR_DATA_DIR");
            std::env::remove_var("SR_LAUNCH_ORDER");
            std::env::remove_var("SR_PROTO_OVERRIDE");
        }
    });
}

static AMBIENT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// Whether this test process may create a child outside every job. `cargo test` runs tests inside
/// cargo's own kill-on-close Job Object, which does **not** allow breakaway, so under cargo the
/// launcher must (and does) fall back to the scheduled task; run the test exe directly to exercise
/// the verified-breakaway path and the nested-job (O1) detection.
pub fn ambient_allows_breakaway() -> bool {
    *AMBIENT.get_or_init(|| {
        use sr_client::win::OwnedProcess;
        // DETACHED | BREAKAWAY_FROM_JOB, suspended: never runs.
        match OwnedProcess::spawn(
            Path::new(LAUNCH),
            "--version",
            0x8 | 0x0100_0000,
            true,
            None,
        ) {
            Ok(p) => {
                let free = p.in_any_job().map(|j| !j).unwrap_or(false);
                let _ = p.terminate(1);
                free
            }
            Err(_) => false,
        }
    })
}

/// Launch method the launcher must pick when the client's own job allows breakaway.
pub fn expected_free_method() -> &'static str {
    if ambient_allows_breakaway() {
        "breakaway"
    } else {
        "task"
    }
}

/// Tests in one binary run one at a time: they spawn many processes and measure timings.
pub fn serial() -> MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// One isolated environment: its own data dir (so its own pipe, locks and task name).
pub struct TestEnv {
    pub dir: PathBuf,
    pub name: String,
}

impl TestEnv {
    pub fn new(name: &str) -> Self {
        init();
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir()
            .join("sr-m1c")
            .join(format!("{name}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self {
            dir,
            name: name.into(),
        }
    }

    pub fn task_name(&self) -> String {
        let sid = sr_client::win::user_sid_string().unwrap();
        sr_client::naming::task_name(&sid, &self.dir)
    }

    pub fn pipe_name(&self) -> String {
        let sid = sr_client::win::user_sid_string().unwrap();
        sr_client::naming::pipe_name(&sid, &self.dir)
    }

    pub fn opts(&self, kind: sr_common::protocol::ConnKind) -> sr_client::AttachOptions {
        let mut o = sr_client::AttachOptions::new(kind, "test");
        o.data_dir = Some(self.dir.clone());
        o.launcher = Some(PathBuf::from(LAUNCH));
        o
    }

    /// Starts the stub daemon directly (not via the launcher) with extra args; waits for its pipe.
    pub fn start_daemon(&self, extra: &[&str]) -> u32 {
        use std::os::windows::process::CommandExt;
        // The daemon is detached on purpose and outlives this handle (cleanup kills it by pid).
        #[allow(clippy::zombie_processes)]
        let child = Command::new(STUB)
            .arg("--data-dir")
            .arg(&self.dir)
            .args(["--launched-by", "test"])
            .args(extra)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(0x0000_0008 | 0x0000_0200) // DETACHED_PROCESS | NEW_PROCESS_GROUP
            .spawn()
            .unwrap();
        let pid = child.id();
        self.wait_owner(pid, Duration::from_secs(10));
        pid
    }

    pub fn wait_owner(&self, pid: u32, timeout: Duration) {
        let dl = Instant::now() + timeout;
        while !self.dir.join(format!("owner-{pid}")).exists() {
            assert!(Instant::now() < dl, "daemon {pid} never opened its pipe");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// PIDs from `<prefix>-<pid>` marker files.
    pub fn markers(&self, prefix: &str) -> BTreeSet<u32> {
        std::fs::read_dir(&self.dir)
            .map(|rd| {
                rd.filter_map(|e| {
                    let n = e.ok()?.file_name().to_string_lossy().into_owned();
                    let rest = n.strip_prefix(prefix)?.strip_prefix('-')?;
                    rest.trim_end_matches(".json").parse().ok()
                })
                .collect()
            })
            .unwrap_or_default()
    }

    /// Stub daemons of this env that are still running.
    pub fn live_daemons(&self) -> BTreeSet<u32> {
        let stubs = pids_named("sr-test-daemon.exe");
        self.markers("start")
            .into_iter()
            .filter(|p| stubs.contains(p))
            .collect()
    }

    pub fn log(&self, file: &str) -> String {
        std::fs::read_to_string(self.dir.join(file)).unwrap_or_default()
    }

    /// Kills this env's daemons and waits until they are gone.
    pub fn kill_daemons(&self) {
        for pid in self.live_daemons() {
            kill(pid);
        }
        let dl = Instant::now() + Duration::from_secs(5);
        while !self.live_daemons().is_empty() && Instant::now() < dl {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        self.kill_daemons();
        let t = self.task_name();
        if t.starts_with(&format!(r"\{TASK_PREFIX}-")) {
            let _ = sr_client::launch::delete_task(&t);
        }
        for _ in 0..20 {
            if std::fs::remove_dir_all(&self.dir).is_ok() || !self.dir.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

pub fn task_exists(name: &str) -> bool {
    let sys = sr_client::win::system_dir().unwrap();
    Command::new(sys.join("schtasks.exe"))
        .args(["/query", "/tn", name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ------------------------------------------------------------------------------------------ MCP

/// A bridge process driven like an MCP client (JSON lines over stdio).
pub struct Bridge {
    pub child: Child,
    stdin: Option<ChildStdin>,
    lines: mpsc::Receiver<String>,
    next_id: u64,
}

impl Bridge {
    pub fn spawn(env: &TestEnv, client: &str, extra_env: &[(&str, &str)]) -> Self {
        let mut c = Command::new(BRIDGE);
        c.args(["--client", client, "--data-dir"])
            .arg(&env.dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (k, v) in extra_env {
            c.env(k, v);
        }
        Self::from_child(c.spawn().unwrap())
    }

    pub fn from_child(mut child: Child) -> Self {
        let out = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for l in BufReader::new(out).lines() {
                let Ok(l) = l else { break };
                if tx.send(l).is_err() {
                    break;
                }
            }
        });
        Self {
            stdin: child.stdin.take(),
            child,
            lines: rx,
            next_id: 0,
        }
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Sends a request and waits for the response with the same id.
    pub fn request(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        let line = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}).to_string();
        let w = self.stdin.as_mut().ok_or("stdin closed")?;
        writeln!(w, "{line}").map_err(|e| e.to_string())?;
        w.flush().map_err(|e| e.to_string())?;
        let dl = Instant::now() + timeout;
        loop {
            let left = dl.saturating_duration_since(Instant::now());
            let l = self
                .lines
                .recv_timeout(left)
                .map_err(|_| format!("no response to {method} within {timeout:?}"))?;
            let v: Value = serde_json::from_str(&l).map_err(|e| format!("{e}: {l}"))?;
            if v["id"] == json!(id) {
                return Ok(v);
            }
        }
    }

    /// `initialize` → the stub's result object (daemon_pid, launched_by, ...).
    pub fn initialize(&mut self, timeout: Duration) -> Result<Value, String> {
        let v = self.request(
            "initialize",
            json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"t","version":"0"}}),
            timeout,
        )?;
        if v.get("error").is_some() {
            return Err(format!("initialize error: {v}"));
        }
        Ok(v["result"].clone())
    }

    pub fn close_stdin(&mut self) {
        self.stdin = None;
    }

    /// Waits for exit; `None` on timeout.
    pub fn wait_exit(&mut self, timeout: Duration) -> Option<i32> {
        let dl = Instant::now() + timeout;
        loop {
            if let Ok(Some(s)) = self.child.try_wait() {
                return Some(s.code().unwrap_or(-1));
            }
            if Instant::now() >= dl {
                return None;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.stdin = None;
        if self.wait_exit(Duration::from_secs(3)).is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

// ------------------------------------------------------------------------------------ processes

#[derive(Debug, Clone)]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    pub name: String,
}

pub fn processes() -> Vec<Proc> {
    let mut v = Vec::new();
    // SAFETY: snapshot handle checked and closed.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return v;
        }
        let mut e: PROCESSENTRY32W = std::mem::zeroed();
        e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snap, &mut e) != 0 {
            loop {
                let n = e.szExeFile.iter().position(|c| *c == 0).unwrap_or(260);
                v.push(Proc {
                    pid: e.th32ProcessID,
                    ppid: e.th32ParentProcessID,
                    name: String::from_utf16_lossy(&e.szExeFile[..n]),
                });
                if Process32NextW(snap, &mut e) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
    }
    v
}

pub fn pids_named(name: &str) -> BTreeSet<u32> {
    processes()
        .into_iter()
        .filter(|p| p.name.eq_ignore_ascii_case(name))
        .map(|p| p.pid)
        .collect()
}

pub fn parent_of(pid: u32) -> Option<Proc> {
    let ps = processes();
    let ppid = ps.iter().find(|p| p.pid == pid)?.ppid;
    ps.into_iter().find(|p| p.pid == ppid)
}

pub fn kill(pid: u32) {
    // SAFETY: handle checked and closed.
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if !h.is_null() {
            TerminateProcess(h, 1);
            CloseHandle(h);
        }
    }
}

pub fn alive(pid: u32) -> bool {
    sr_client::win::pid_alive(pid)
}

pub fn resume_all_threads(pid: u32) {
    // SAFETY: snapshot/thread handles checked and closed.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        let mut te: THREADENTRY32 = std::mem::zeroed();
        te.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        if Thread32First(snap, &mut te) != 0 {
            loop {
                if te.th32OwnerProcessID == pid {
                    let th = OpenThread(THREAD_SUSPEND_RESUME, 0, te.th32ThreadID);
                    if !th.is_null() {
                        ResumeThread(th);
                        CloseHandle(th);
                    }
                }
                if Thread32Next(snap, &mut te) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
    }
}

/// A Job Object; closing it (drop) kills members when `KILL_ON_JOB_CLOSE` is set.
pub struct Job(pub HANDLE);
// SAFETY: a job handle can be used from any thread.
unsafe impl Send for Job {}

impl Job {
    pub fn new(limit_flags: u32) -> Self {
        // SAFETY: plain job creation and limit setting.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            assert!(!job.is_null());
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = limit_flags;
            let ok = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            assert_ne!(ok, 0);
            Job(job)
        }
    }

    pub fn assign(&self, pid: u32) {
        // SAFETY: handle checked and closed.
        unsafe {
            let h = OpenProcess(PROCESS_ALL_ACCESS, 0, pid);
            assert!(!h.is_null());
            let ok = AssignProcessToJobObject(self.0, h);
            CloseHandle(h);
            assert_ne!(ok, 0, "AssignProcessToJobObject failed");
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: owned handle.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// Median and p95 of millisecond samples.
pub fn stats(mut v: Vec<f64>) -> (f64, f64, f64) {
    v.sort_by(|a, b| a.total_cmp(b));
    let n = v.len();
    (v[0], v[n / 2], v[((n * 95) / 100).min(n - 1)])
}

/// Appends a result line to target/m1-c/logs/test-results.txt (read by the report).
pub fn record(line: &str) {
    eprintln!("{line}");
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/m1-c/logs/test-results.txt");
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(p)
    {
        let _ = writeln!(f, "{} {line}", sr_common::events::now_ms());
    }
}
