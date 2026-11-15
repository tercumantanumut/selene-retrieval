//! A Job Object that kills the whole CLI process tree when it is dropped (M5-C).
//!
//! `claude.cmd` / `codex.cmd` run `cmd.exe` → `node` → the CLI, which may start helpers; killing
//! only the direct child would leave them running (and generating). Every CLI run is put in its
//! own job with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`; dropping the [`Job`] (cancel, timeout, the
//! caller dropping the stream, the daemon exiting) ends every process in it. Nested jobs work on
//! Windows 8+, so this also works when the daemon itself runs in a job.
//!
//! The process is assigned right after it starts; a CLI needs far longer than that to start
//! children of its own.

#[cfg(windows)]
#[allow(unsafe_code)] // Win32 FFI: CreateJobObjectW / SetInformationJobObject / AssignProcessToJobObject / GetSystemDirectoryW. Reviewed (M5-C).
mod imp {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };

    /// An owned job handle.
    pub struct Job(HANDLE);

    // SAFETY: a job handle is a kernel handle; it may be used and closed from any thread.
    unsafe impl Send for Job {}
    // SAFETY: as above; the handle is only read after creation.
    unsafe impl Sync for Job {}

    impl Job {
        /// A new kill-on-close job, or `None` if Windows refused.
        pub fn new() -> Option<Self> {
            // SAFETY: both pointers may be null (no name, default security).
            let h = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if h.is_null() {
                return None;
            }
            let job = Job(h);
            // SAFETY: an all-zero JOBOBJECT_EXTENDED_LIMIT_INFORMATION is valid ("no limits").
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: `info` lives across the call and the size matches the class.
            let ok = unsafe {
                SetInformationJobObject(
                    job.0,
                    JobObjectExtendedLimitInformation,
                    (&raw const info).cast(),
                    u32::try_from(std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                        .unwrap_or(u32::MAX),
                )
            };
            (ok != 0).then_some(job)
        }

        /// Puts the process behind `process` (a live child's handle) into the job.
        pub fn assign(&self, process: HANDLE) -> bool {
            if process.is_null() {
                return false;
            }
            // SAFETY: both handles are valid for the duration of the call (the caller holds the
            // child, so its handle is open).
            unsafe { AssignProcessToJobObject(self.0, process) != 0 }
        }
    }

    /// `%SystemRoot%\System32` from Windows itself (not from the environment).
    pub fn system_dir() -> Option<std::path::PathBuf> {
        use std::os::windows::ffi::OsStringExt;
        use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
        let mut buf = [0u16; 512];
        // SAFETY: the buffer and its length match; the call writes at most that many units.
        let n = unsafe { GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } as usize;
        (n > 0 && n < buf.len())
            .then(|| std::path::PathBuf::from(std::ffi::OsString::from_wide(&buf[..n])))
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: owned handle, closed once. With KILL_ON_JOB_CLOSE this ends the tree.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

#[cfg(windows)]
pub(crate) use imp::{Job, system_dir};

#[cfg(windows)]
impl Job {
    /// Assign a tokio child; `false` when it already exited or Windows refused.
    pub(crate) fn assign_child(&self, child: &tokio::process::Child) -> bool {
        child.raw_handle().is_some_and(|h| self.assign(h.cast()))
    }
}

/// Unix: the CLI runs in a process group of its own ([`super::process::command`] sets
/// `process_group(0)`, so the group id is the child's pid). The job sends `SIGKILL` to the whole
/// group once, so helpers the CLI started don't outlive the run: at the end of [`Job::wait`],
/// on cancel or timeout ([`Job::kill_tree`]) and on drop.
///
/// The group id is the leader's pid, which may be reused once the leader is reaped. So the group
/// is only ever killed while the leader is not yet reaped: [`Job::wait`] kills it after the
/// leader exited (a zombie still holds the pid) and before `wait` reaps it, and no kill follows.
///
/// No `KILL_ON_JOB_CLOSE` twin: if the daemon itself crashes, the CLI tree keeps running.
#[cfg(unix)]
pub(crate) struct Job {
    /// The group to kill; 0 once it was killed (or the leader reaped): never killed again.
    pgid: std::sync::atomic::AtomicI32,
}

#[cfg(unix)]
#[allow(unsafe_code)] // libc FFI: killpg, waitid. Reviewed (macOS port).
impl Job {
    pub(crate) fn new() -> Option<Self> {
        Some(Job {
            pgid: std::sync::atomic::AtomicI32::new(0),
        })
    }

    /// Remember the child's process group; `false` when it already exited.
    pub(crate) fn assign_child(&self, child: &tokio::process::Child) -> bool {
        match child.id().and_then(|p| i32::try_from(p).ok()) {
            Some(pid) if pid > 1 => {
                self.pgid.store(pid, std::sync::atomic::Ordering::SeqCst);
                true
            }
            _ => false,
        }
    }

    /// Kills the whole group, once. Call it only while the leader (the `Child` we hold) is not
    /// yet reaped.
    pub(crate) fn kill_tree(&self) {
        let pgid = self.pgid.swap(0, std::sync::atomic::Ordering::SeqCst);
        if pgid > 1 {
            // SAFETY: plain syscall with no pointers; `pgid` is the group our unreaped child
            // leads, so the id can't belong to anyone else.
            unsafe {
                libc::killpg(pgid, libc::SIGKILL);
            }
        }
    }

    /// Waits for `child` (the leader): first until it has exited without reaping it (`waitid`
    /// with `WNOWAIT`), then kills what is left of its group, then reaps it. Cancel-safe: if this
    /// is dropped before the reap, the group can still be killed safely.
    pub(crate) async fn wait(
        &self,
        child: &mut tokio::process::Child,
    ) -> std::io::Result<std::process::ExitStatus> {
        let pid = self.pgid.load(std::sync::atomic::Ordering::SeqCst);
        if pid > 1 {
            let exited = tokio::task::spawn_blocking(move || wait_exited(pid)).await;
            match exited {
                // ECHILD: already reaped (can't happen while we hold the child): never kill.
                Ok(Err(e)) if e.raw_os_error() == Some(libc::ECHILD) => {
                    self.pgid.store(0, std::sync::atomic::Ordering::SeqCst);
                }
                // Exited (or the wait failed): the leader is not reaped yet, so the id is ours.
                _ => self.kill_tree(),
            }
        }
        child.wait().await
    }
}

/// Blocks until process `pid` (our child) has exited, without reaping it.
#[cfg(unix)]
#[allow(unsafe_code)] // libc FFI: waitid. Reviewed (macOS port).
fn wait_exited(pid: i32) -> std::io::Result<()> {
    let id = libc::id_t::try_from(pid).map_err(|_| std::io::Error::other("bad pid"))?;
    loop {
        // SAFETY: an all-zero siginfo_t is a valid out value.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: `info` outlives the call; WNOWAIT leaves the child waitable (not reaped).
        let rc = unsafe { libc::waitid(libc::P_PID, id, &mut info, libc::WEXITED | libc::WNOWAIT) };
        if rc == 0 {
            return Ok(());
        }
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

#[cfg(unix)]
impl Drop for Job {
    fn drop(&mut self) {
        self.kill_tree();
    }
}
