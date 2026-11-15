//! macOS sandbox primitives for the extract worker (Amendment 18.2), with the API of the Windows
//! module so the pool and worker code stay the same. This is the only module of the crate on
//! macOS allowed to use `unsafe` (libc and libSystem calls, each with its safety argument).
//!
//! | Windows | macOS |
//! |---|---|
//! | AppContainer, zero capabilities | Seatbelt deny-default profile the worker applies to itself before it reads any request (`sr-extract` src/seatbelt.rs), **checked here** with `sandbox_check` on the worker's pid before it gets work ([`verify_worker`]) |
//! | Job memory cap (commit) | watchdog thread polling `proc_pid_rusage` `ri_phys_footprint`, kill at the same cap |
//! | Job CPU limit, re-armed per request | the same watchdog: user CPU since the request started over the budget → kill; `RLIMIT_CPU` as a lifetime ceiling the worker can't raise ([`CPU_LIFETIME`]; the pool recycles at half) |
//! | `ActiveProcessLimit = 1`, child-process policy | profile denies fork/exec; `RLIMIT_NPROC = 0` |
//! | `TerminateJobObject`, kill on close | `killpg` + `kill` (`SIGKILL`) of the worker's own process group, only while it is unreaped (so the pid can't have been reused) |
//! | explicit handle list | three pipes; every other descriptor that isn't close-on-exec is closed in the child |
//! | minimal environment, `cwd = System32` | empty environment, `cwd = /` |
//!
//! There is one mode: if the sandbox can't be applied or verified the worker is killed and the
//! spawn fails (fail closed); nothing ever runs unconfined.
#![allow(unsafe_code)]

use std::ffi::{CString, OsStr, c_char, c_int};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use sr_common::protocol::extract::WorkerReady;

/// Exit code reported when the supervisor killed the worker (same value as on Windows).
pub const EXIT_KILLED: u32 = 0xE7E7;
/// The worker was reaped by someone else and its status is lost (should never happen).
const EXIT_LOST: u32 = 0xFFFF;

// Job message codes (the Windows values, so the shared classification code works unchanged).
pub const MSG_END_OF_JOB_TIME: u32 = 1;
pub const MSG_END_OF_PROCESS_TIME: u32 = 2;
pub const MSG_ABNORMAL_EXIT_PROCESS: u32 = 8;
pub const MSG_PROCESS_MEMORY_LIMIT: u32 = 9;
pub const MSG_JOB_MEMORY_LIMIT: u32 = 10;

/// Lifetime CPU ceiling of one worker (`RLIMIT_CPU`, user + system), set before exec so the
/// worker can't raise it. It also stops a worker whose supervisor died (macOS has no
/// kill-on-close). `SIGXCPU` at this value, `SIGKILL` [`CPU_HARD_GRACE_SECS`] later.
pub const CPU_LIFETIME: Duration = Duration::from_secs(30 * 60);
const CPU_HARD_GRACE_SECS: u64 = 10;
/// The pool recycles a worker after a job once it has used this much CPU, so the ceiling never
/// hits a healthy worker (one job is bounded by its timeout, at most 2 minutes).
pub const CPU_RECYCLE: Duration = Duration::from_secs(15 * 60);

/// Watchdog poll interval while the worker is busy (a request is new or it is using CPU), and
/// while it is idle.
const POLL_BUSY: Duration = Duration::from_millis(20);
const POLL_IDLE: Duration = Duration::from_millis(250);
/// After a request starts the watchdog polls fast for at least this long.
const BUSY_AFTER_ARM: Duration = Duration::from_secs(2);

/// Most descriptors the child looks at when closing inherited ones.
const MAX_INHERITED_FD: libc::rlim_t = 65_536;

// ------------------------------------------------------------------ shared process state

/// Everything the job, its killer, the watchdog and the child handle share. One lock covers the
/// kill and the reap, so a kill can never reach a pid that was reaped and reused.
#[derive(Debug)]
struct State {
    /// The worker's pid (also its process group id); 0 until spawned.
    pid: libc::pid_t,
    /// Reaped (by [`OwnedHandle::wait_exit`]); no signal is sent after this.
    reaped: bool,
    /// Raw wait status once reaped (`None`: lost).
    status: Option<c_int>,
    /// We sent the kill.
    killed: bool,
    memory_limit: u64,
    cpu_budget: Duration,
    /// User CPU of the worker when the budget was armed.
    cpu_base: Duration,
    armed_at: Instant,
    msgs: Vec<u32>,
    xcpu_reported: bool,
    stop: bool,
}

#[derive(Debug)]
struct Inner {
    st: Mutex<State>,
    cv: Condvar,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// `SIGKILL` to the worker's process group and to the worker itself, unless it is already reaped.
fn kill_locked(st: &mut State) {
    if st.pid > 0 && !st.reaped {
        // SAFETY: plain signal calls. `pid` is our child and not yet reaped, so neither it nor
        // the process group it leads can belong to another process.
        unsafe {
            libc::killpg(st.pid, libc::SIGKILL);
            libc::kill(st.pid, libc::SIGKILL);
        }
        st.killed = true;
    }
}

/// Resource usage of a live worker.
#[derive(Debug, Clone, Copy)]
struct Usage {
    footprint: u64,
    peak_footprint: u64,
    user: Duration,
    system: Duration,
}

#[repr(C)]
struct MachTimebase {
    numer: u32,
    denom: u32,
}

unsafe extern "C" {
    fn mach_timebase_info(info: *mut MachTimebase) -> c_int;
    // libsystem_sandbox, not in the public header (Chromium and WebKit declare it the same way):
    // 1 = `operation` (with its filter argument) is denied to `pid`, 0 = allowed, -1 = error.
    // A null operation asks whether `pid` is sandboxed at all.
    fn sandbox_check(pid: libc::pid_t, operation: *const c_char, filter_type: c_int, ...) -> c_int;
    /// Flag for `filter_type`: don't log the check as a violation.
    static SANDBOX_CHECK_NO_REPORT: c_int;
}

const SANDBOX_FILTER_NONE: c_int = 0;
const SANDBOX_FILTER_PATH: c_int = 1;
const SANDBOX_FILTER_GLOBAL_NAME: c_int = 2;

/// Mach time units (what `proc_pid_rusage` reports CPU in on Apple silicon) → nanoseconds.
fn mach_to_duration(t: u64) -> Duration {
    static TB: OnceLock<(u64, u64)> = OnceLock::new();
    let (n, d) = *TB.get_or_init(|| {
        let mut tb = MachTimebase { numer: 0, denom: 0 };
        // SAFETY: `tb` is a valid out value of the declared layout.
        let rc = unsafe { mach_timebase_info(&mut tb) };
        if rc != 0 || tb.denom == 0 {
            (1, 1)
        } else {
            (u64::from(tb.numer), u64::from(tb.denom))
        }
    });
    Duration::from_nanos(
        u64::try_from(u128::from(t) * u128::from(n) / u128::from(d)).unwrap_or(u64::MAX),
    )
}

/// Usage of `pid`; the caller holds the state lock and has checked it is unreaped.
fn usage(pid: libc::pid_t) -> Option<Usage> {
    // SAFETY: an all-zero rusage_info_v4 is a valid out value (plain integers).
    let mut ri: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    // SAFETY: the buffer is a rusage_info_v4, which is what the V4 flavor writes.
    let rc = unsafe {
        libc::proc_pid_rusage(
            pid,
            libc::RUSAGE_INFO_V4,
            (&mut ri as *mut libc::rusage_info_v4).cast::<libc::rusage_info_t>(),
        )
    };
    (rc == 0).then(|| Usage {
        footprint: ri.ri_phys_footprint,
        peak_footprint: ri.ri_lifetime_max_phys_footprint,
        user: mach_to_duration(ri.ri_user_time),
        system: mach_to_duration(ri.ri_system_time),
    })
}

/// The watchdog: memory cap and per-request CPU budget.
fn watch(inner: &Inner) {
    let mut last_cpu: Option<Duration> = None;
    let mut cpu_moved_at = Instant::now();
    let mut g = inner.lock();
    loop {
        if g.stop {
            return;
        }
        if g.pid > 0
            && !g.reaped
            && !g.killed
            && let Some(u) = usage(g.pid)
        {
            if u.footprint > g.memory_limit {
                g.msgs.push(MSG_PROCESS_MEMORY_LIMIT);
                kill_locked(&mut g);
            } else if u.user.saturating_sub(g.cpu_base) > g.cpu_budget {
                g.msgs.push(MSG_END_OF_JOB_TIME);
                kill_locked(&mut g);
            }
            let cpu = u.user + u.system;
            if last_cpu != Some(cpu) {
                cpu_moved_at = Instant::now();
            }
            last_cpu = Some(cpu);
        }
        // Fast while a request is new or the worker is using CPU; slow when it sits idle.
        let busy = g.armed_at.elapsed() < BUSY_AFTER_ARM
            || cpu_moved_at.elapsed() < Duration::from_secs(1);
        let nap = if busy { POLL_BUSY } else { POLL_IDLE };
        g = inner
            .cv
            .wait_timeout(g, nap)
            .map(|(g, _)| g)
            .unwrap_or_else(|p| p.into_inner().0);
    }
}

/// The worker "handle": shared state that waits for and reaps the process.
#[derive(Debug)]
pub struct OwnedHandle(Arc<Inner>);

impl OwnedHandle {
    fn wait_exit(&self, timeout: Duration) -> Option<u32> {
        let deadline = Instant::now() + timeout;
        let mut nap = Duration::from_millis(1);
        loop {
            {
                let mut g = self.0.lock();
                if g.reaped {
                    return Some(g.status.map_or(EXIT_LOST, |s| exit_code(s, g.killed)));
                }
                if g.pid <= 0 {
                    return None;
                }
                let mut status: c_int = 0;
                // SAFETY: `status` is a valid out value; WNOHANG never blocks.
                let r = unsafe { libc::waitpid(g.pid, &mut status, libc::WNOHANG) };
                if r == g.pid {
                    g.reaped = true;
                    g.status = Some(status);
                    self.0.cv.notify_all();
                    return Some(exit_code(status, g.killed));
                }
                if r == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD) {
                    // Not our child any more: never signal that pid again.
                    g.reaped = true;
                    return Some(EXIT_LOST);
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            std::thread::sleep(nap.min(deadline - now));
            nap = (nap * 2).min(Duration::from_millis(20));
        }
    }
}

/// Exit code from a wait status: the exit status, [`EXIT_KILLED`] for our own kill, or
/// `128 + signal` for any other signal (the shell convention).
fn exit_code(status: c_int, killed: bool) -> u32 {
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status) as u32
    } else if libc::WIFSIGNALED(status) {
        let sig = libc::WTERMSIG(status);
        if sig == libc::SIGKILL && killed {
            EXIT_KILLED
        } else {
            128 + sig as u32
        }
    } else {
        EXIT_LOST
    }
}

// ------------------------------------------------------------------ job

/// The worker's limits and its watchdog thread. Dropping it kills the worker.
pub struct Job {
    inner: Arc<Inner>,
    watchdog: Option<JoinHandle<()>>,
}

/// Kills the worker from another thread (the wall-clock watchdog, pool shutdown).
#[derive(Debug, Clone)]
pub struct JobKiller(Arc<Inner>);

impl JobKiller {
    pub fn kill(&self) {
        kill_locked(&mut self.0.lock());
    }
}

impl Job {
    pub fn new(memory_limit: u64, cpu_budget: Duration) -> io::Result<Job> {
        let inner = Arc::new(Inner {
            st: Mutex::new(State {
                pid: 0,
                reaped: false,
                status: None,
                killed: false,
                memory_limit,
                cpu_budget,
                cpu_base: Duration::ZERO,
                armed_at: Instant::now(),
                msgs: Vec::new(),
                xcpu_reported: false,
                stop: false,
            }),
            cv: Condvar::new(),
        });
        let w = Arc::clone(&inner);
        let watchdog = std::thread::Builder::new()
            .name("sr-extract-limits".into())
            .spawn(move || watch(&w))?;
        Ok(Job {
            inner,
            watchdog: Some(watchdog),
        })
    }

    pub fn killer(&self) -> JobKiller {
        JobKiller(Arc::clone(&self.inner))
    }

    /// Re-arms the CPU budget for the next request, relative to the CPU already used (the
    /// Windows JOB_TIME semantics).
    pub fn rearm_cpu(&mut self, budget: Duration) {
        let mut g = self.inner.lock();
        if g.pid > 0 && !g.reaped {
            g.cpu_base = usage(g.pid).map_or(g.cpu_base, |u| u.user);
        }
        g.cpu_budget = budget;
        g.armed_at = Instant::now();
        self.inner.cv.notify_all();
    }

    pub fn terminate(&self) {
        kill_locked(&mut self.inner.lock());
    }

    /// Highest footprint of the worker since it started (kernel-tracked, never decreases).
    pub fn peak_process_memory(&self) -> u64 {
        self.live_usage().map_or(0, |u| u.peak_footprint)
    }

    /// User-mode CPU time of the worker.
    pub fn user_time(&self) -> Duration {
        self.live_usage().map_or(Duration::ZERO, |u| u.user)
    }

    /// Whether the worker has used enough CPU to be recycled (see [`CPU_RECYCLE`]).
    pub(crate) fn needs_recycle(&self) -> bool {
        self.live_usage()
            .is_some_and(|u| u.user + u.system >= CPU_RECYCLE)
    }

    /// Limit messages since the last call; a worker that died of `SIGXCPU` (the lifetime ceiling)
    /// reports the end of its CPU time once.
    pub fn drain_messages(&self) -> Vec<u32> {
        let mut g = self.inner.lock();
        let mut v = std::mem::take(&mut g.msgs);
        if !g.xcpu_reported
            && g.status
                .is_some_and(|s| libc::WIFSIGNALED(s) && libc::WTERMSIG(s) == libc::SIGXCPU)
        {
            g.xcpu_reported = true;
            v.push(MSG_END_OF_PROCESS_TIME);
        }
        v
    }

    fn live_usage(&self) -> Option<Usage> {
        let g = self.inner.lock();
        if g.pid > 0 && !g.reaped {
            usage(g.pid)
        } else {
            None
        }
    }

    fn assign(&self, pid: libc::pid_t) {
        let mut g = self.inner.lock();
        g.pid = pid;
        g.armed_at = Instant::now();
        self.inner.cv.notify_all();
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        {
            let mut g = self.inner.lock();
            kill_locked(&mut g);
            g.stop = true;
            self.inner.cv.notify_all();
        }
        if let Some(t) = self.watchdog.take() {
            let _ = t.join();
        }
    }
}

impl std::fmt::Debug for Job {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Job")
            .field("pid", &self.inner.lock().pid)
            .finish()
    }
}

// ------------------------------------------------------------------ AppContainer (Windows only)

/// Windows-only concept; [`AppContainer::ensure`] always fails here (the pool never calls it on
/// macOS).
#[derive(Debug)]
pub struct AppContainer {
    pub name: String,
}

impl AppContainer {
    pub fn ensure(_name: &str, _display: &str) -> io::Result<AppContainer> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "AppContainer exists only on Windows",
        ))
    }

    pub fn sid_string(&self) -> io::Result<String> {
        Err(io::ErrorKind::Unsupported.into())
    }
}

/// Nothing to delete on macOS.
pub fn delete_app_container_profile(_name: &str) -> io::Result<()> {
    Ok(())
}

/// No install-directory ACL on macOS (the Seatbelt profile reads nothing there). "Unchanged".
pub fn ensure_app_packages_read(_dir: &Path) -> io::Result<bool> {
    Ok(false)
}

// ------------------------------------------------------------------ process

/// Sandbox the worker runs in. Only [`TokenKind::Seatbelt`] exists on macOS; the Windows names
/// are kept for the shared pool code and are refused by [`spawn`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    AppContainer,
    LowIntegrity,
    Seatbelt,
}

/// A running worker: its handle, pid and the parent's ends of the three pipes. Dropping it kills
/// and reaps the worker if nothing else did.
#[derive(Debug)]
pub struct ChildProcess {
    pub process: OwnedHandle,
    pub pid: u32,
    pub stdin: File,
    pub stdout: File,
    pub stderr: File,
}

impl ChildProcess {
    /// Waits up to `timeout` for exit; returns the exit code if it exited (and reaps it).
    pub fn wait_exit(&self, timeout: Duration) -> Option<u32> {
        self.process.wait_exit(timeout)
    }
}

impl Drop for ChildProcess {
    fn drop(&mut self) {
        if self.process.wait_exit(Duration::ZERO).is_none() {
            kill_locked(&mut self.process.0.lock());
            let _ = self.process.wait_exit(Duration::from_secs(5));
        }
    }
}

/// One past the highest descriptor the child may have inherited: the soft `RLIMIT_NOFILE`, capped.
fn fd_limit() -> c_int {
    let mut rl = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `rl` is a valid out value.
    let soft = if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) } == 0 {
        rl.rlim_cur.min(MAX_INHERITED_FD)
    } else {
        MAX_INHERITED_FD
    };
    c_int::try_from(soft).unwrap_or(c_int::MAX)
}

/// Starts `exe args…` in its own process group with the limits above, an empty environment,
/// `cwd = /`, the three pipes and no other inherited descriptor. The worker applies its Seatbelt
/// profile itself before the ready frame; [`verify_worker`] checks it from outside.
#[allow(clippy::zombie_processes)] // reaped with waitpid by OwnedHandle::wait_exit
pub fn spawn(
    exe: &Path,
    args: &[&OsStr],
    kind: TokenKind,
    _app_container: Option<&AppContainer>,
    job: &Job,
) -> io::Result<ChildProcess> {
    if kind != TokenKind::Seatbelt {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "only the Seatbelt sandbox exists on macOS",
        ));
    }
    let limit = fd_limit();
    let cpu_soft = CPU_LIFETIME.as_secs();
    let cpu_hard = cpu_soft + CPU_HARD_GRACE_SECS;
    let mut cmd = Command::new(exe);
    cmd.args(args)
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    // SAFETY: the hook runs in the child between fork and exec. It allocates nothing and calls
    // only setrlimit, fcntl and close, which are async-signal-safe; `io::Error::last_os_error`
    // only reads errno. Close-on-exec descriptors are left alone: exec closes them, and one is
    // std's pipe that reports a failed exec.
    unsafe {
        cmd.pre_exec(move || {
            let set = |res: c_int, soft: libc::rlim_t, hard: libc::rlim_t| {
                let rl = libc::rlimit {
                    rlim_cur: soft,
                    rlim_max: hard,
                };
                if libc::setrlimit(res, &rl) == 0 {
                    Ok(())
                } else {
                    Err(io::Error::last_os_error())
                }
            };
            set(libc::RLIMIT_CPU, cpu_soft, cpu_hard)?;
            set(libc::RLIMIT_NPROC, 0, 0)?;
            set(libc::RLIMIT_CORE, 0, 0)?;
            set(libc::RLIMIT_FSIZE, 0, 0)?;
            for fd in 3..limit {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags != -1 && flags & libc::FD_CLOEXEC == 0 {
                    libc::close(fd);
                }
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    let pid =
        libc::pid_t::try_from(child.id()).map_err(|_| io::Error::other("pid out of range"))?;
    job.assign(pid);
    let handle = OwnedHandle(Arc::clone(&job.inner));
    let pipes = (child.stdin.take(), child.stdout.take(), child.stderr.take());
    let (Some(i), Some(o), Some(e)) = pipes else {
        kill_locked(&mut job.inner.lock());
        let _ = handle.wait_exit(Duration::from_secs(5));
        return Err(io::Error::other("worker pipes missing"));
    };
    let stdin = File::from(OwnedFd::from(i));
    // A dead worker turns our next write into EPIPE, never a SIGPIPE in the daemon.
    // SAFETY: valid descriptor we own; F_SETNOSIGPIPE takes an int.
    unsafe { libc::fcntl(stdin.as_raw_fd(), libc::F_SETNOSIGPIPE, 1) };
    Ok(ChildProcess {
        process: handle,
        pid: child.id(),
        stdin,
        stdout: File::from(OwnedFd::from(o)),
        stderr: File::from(OwnedFd::from(e)),
    })
}

// ------------------------------------------------------------------ verification

/// One denial the worker's sandbox must enforce.
fn denied(pid: libc::pid_t, op: &str, filter: c_int, arg: Option<&[u8]>) -> Result<(), String> {
    let op_c = CString::new(op).map_err(|_| "bad operation name".to_string())?;
    // SAFETY: reading an exported constant of libsystem_sandbox.
    let no_report = unsafe { SANDBOX_CHECK_NO_REPORT };
    let rc = match arg {
        // SAFETY: NUL-terminated strings that outlive the call; the filter types used here take
        // no argument (NONE) or one C string (PATH, GLOBAL_NAME).
        None => unsafe { sandbox_check(pid, op_c.as_ptr(), filter | no_report) },
        Some(a) => {
            let a_c = CString::new(a).map_err(|_| "bad check argument".to_string())?;
            // SAFETY: as above.
            unsafe { sandbox_check(pid, op_c.as_ptr(), filter | no_report, a_c.as_ptr()) }
        }
    };
    match rc {
        1 => Ok(()),
        0 => Err(format!(
            "{op} {} is allowed",
            arg.map(String::from_utf8_lossy).unwrap_or_default()
        )),
        _ => Err(format!("sandbox_check({op}) failed ({rc})")),
    }
}

/// Checks from outside, on the worker's pid, that its sandbox is in force and denies what it
/// must: no file reads outside the font folders (home, the per-user temp and socket folders,
/// `/etc`, its own folder), no file writes, no network, no fork or exec, no Mach services. Any
/// failure → `Err` and the caller kills the worker (fail closed). The worker's own report must
/// agree, but is never enough on its own.
pub(crate) fn verify_worker(
    child: &ChildProcess,
    ready: &WorkerReady,
    exe: &Path,
) -> io::Result<()> {
    let fail = |why: String| {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("the file reader's sandbox is not in force: {why}"),
        ))
    };
    if ready.integrity != "seatbelt" {
        return fail(format!(
            "the worker reports sandbox \"{}\"",
            ready.integrity
        ));
    }
    let Ok(pid) = libc::pid_t::try_from(child.pid) else {
        return fail("pid out of range".into());
    };
    // SAFETY: a null operation with filter type NONE takes no further arguments.
    let any = unsafe { sandbox_check(pid, std::ptr::null(), SANDBOX_FILTER_NONE) };
    if any != 1 {
        return fail(format!("the kernel reports no sandbox ({any})"));
    }
    let mut read_paths: Vec<Vec<u8>> = vec![
        b"/Users".to_vec(),
        b"/private/etc/passwd".to_vec(),
        b"/private/var/folders".to_vec(),
        b"/Volumes".to_vec(),
    ];
    let mut write_paths: Vec<Vec<u8>> = vec![
        b"/private/tmp/sr-extract-check".to_vec(),
        b"/private/var/folders".to_vec(),
        b"/Users".to_vec(),
    ];
    if let Some(dir) = exe.parent() {
        read_paths.push(dir.join("x").as_os_str().as_bytes().to_vec());
        write_paths.push(dir.join("x").as_os_str().as_bytes().to_vec());
    }
    if let Some(home) = std::env::var_os("HOME").filter(|h| Path::new(h).is_absolute()) {
        let h = Path::new(&home).join("x");
        read_paths.push(h.as_os_str().as_bytes().to_vec());
        write_paths.push(h.as_os_str().as_bytes().to_vec());
    }
    let mut checks: Vec<(&str, c_int, Option<Vec<u8>>)> = vec![
        ("network-outbound", SANDBOX_FILTER_NONE, None),
        ("network-inbound", SANDBOX_FILTER_NONE, None),
        ("network-bind", SANDBOX_FILTER_NONE, None),
        ("process-fork", SANDBOX_FILTER_NONE, None),
        // The kernel answers exec checks only under "process-exec*" with a path (macOS 26+).
        (
            "process-exec*",
            SANDBOX_FILTER_PATH,
            Some(b"/bin/sh".to_vec()),
        ),
        (
            "mach-lookup",
            SANDBOX_FILTER_GLOBAL_NAME,
            Some(b"com.apple.pasteboard.1".to_vec()),
        ),
        (
            "mach-lookup",
            SANDBOX_FILTER_GLOBAL_NAME,
            Some(b"com.apple.coreservices.launchservicesd".to_vec()),
        ),
    ];
    checks.extend(
        read_paths
            .into_iter()
            .map(|p| ("file-read-data", SANDBOX_FILTER_PATH, Some(p))),
    );
    checks.extend(
        write_paths
            .into_iter()
            .map(|p| ("file-write-data", SANDBOX_FILTER_PATH, Some(p))),
    );
    for (op, filter, arg) in &checks {
        if let Err(why) = denied(pid, op, *filter, arg.as_deref()) {
            return fail(why);
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ identity

/// A worker process identity that survives pid reuse: pid plus start time (microseconds).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WorkerId {
    pub pid: u32,
    pub created: u64,
}

/// `(start time in µs, zombie)` of `pid`, if it exists.
fn bsd_info(pid: u32) -> Option<(u64, bool)> {
    let pid = c_int::try_from(pid).ok()?;
    // SAFETY: an all-zero proc_bsdinfo is a valid out value (integers and byte arrays).
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = c_int::try_from(size_of::<libc::proc_bsdinfo>()).ok()?;
    // SAFETY: the buffer is a proc_bsdinfo of `size` bytes, what PROC_PIDTBSDINFO writes.
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    (n == size).then(|| {
        (
            info.pbi_start_tvsec
                .saturating_mul(1_000_000)
                .saturating_add(info.pbi_start_tvusec),
            info.pbi_status == libc::SZOMB,
        )
    })
}

impl WorkerId {
    /// Identity of the worker behind `h`.
    pub fn of(_h: &OwnedHandle, pid: u32) -> WorkerId {
        WorkerId {
            pid,
            created: bsd_info(pid).map_or(0, |(t, _)| t),
        }
    }

    /// Whether this exact process (same pid **and** start time) is still running. A recycled
    /// pid that now belongs to another process, or a zombie, reports `false`.
    pub fn is_alive(&self) -> bool {
        bsd_info(self.pid).is_some_and(|(t, zombie)| t == self.created && !zombie)
    }
}

/// Last OS error code (diagnostics).
pub fn last_error_code() -> u32 {
    io::Error::last_os_error()
        .raw_os_error()
        .map_or(0, |c| c as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_follow_the_shell_convention() {
        // Exit status 3: (3 << 8). Killed by SIGSEGV (11): 11. SIGKILL (9): 9.
        assert_eq!(exit_code(3 << 8, false), 3);
        assert_eq!(exit_code(11, false), 128 + 11);
        assert_eq!(exit_code(9, true), EXIT_KILLED);
        assert_eq!(exit_code(9, false), 128 + 9);
    }

    #[test]
    fn mach_time_converts_to_real_time() {
        // 1 s of mach time is between 1 ms and 1000 s whatever the timebase (sanity, not exact).
        let d = mach_to_duration(1_000_000_000);
        assert!(
            d > Duration::from_millis(1) && d < Duration::from_secs(1000),
            "{d:?}"
        );
    }

    #[test]
    fn an_unsandboxed_process_fails_verification() {
        // This test process is not sandboxed: the kernel check must say so.
        // SAFETY: null operation, filter NONE, no further arguments.
        let any = unsafe { sandbox_check(libc::getpid(), std::ptr::null(), SANDBOX_FILTER_NONE) };
        assert_eq!(any, 0);
        let me = libc::pid_t::try_from(std::process::id()).unwrap_or(0);
        assert!(denied(me, "file-read-data", SANDBOX_FILTER_PATH, Some(b"/Users")).is_err());
    }

    #[test]
    fn identity_of_this_process() {
        let me = std::process::id();
        let (t, zombie) = bsd_info(me).unwrap_or((0, true));
        assert!(t > 0 && !zombie);
        let id = WorkerId {
            pid: me,
            created: t,
        };
        assert!(id.is_alive());
        assert!(
            !WorkerId {
                pid: me,
                created: t + 1
            }
            .is_alive()
        );
    }
}
