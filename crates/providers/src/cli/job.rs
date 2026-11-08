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

/// Non-Windows builds (never shipped): no job; the child is still killed on drop.
#[cfg(not(windows))]
pub(crate) struct Job;

#[cfg(not(windows))]
impl Job {
    pub(crate) fn new() -> Option<Self> {
        Some(Job)
    }
    pub(crate) fn assign_child(&self, _child: &tokio::process::Child) -> bool {
        true
    }
}

#[cfg(not(windows))]
pub(crate) fn system_dir() -> Option<std::path::PathBuf> {
    None
}
