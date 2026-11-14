//! Win32 sandbox primitives for the extract worker: Job Object, anonymous pipes, low-integrity
//! restricted token, AppContainer profile, process creation with a locked-down attribute list,
//! and the install-directory ACL used by AppContainer.
//!
//! This is the only module of the crate allowed to use `unsafe` (reviewed Win32 FFI). Launch
//! flags are part of the contract (M0-C): `DETACHED_PROCESS` (never `CREATE_NO_WINDOW`, which
//! makes Windows start a conhost.exe that the child-process ban blocks → 0xC0000142), an explicit
//! handle allowlist, `CHILD_PROCESS_RESTRICTED`, ACG and image-load mitigations, a minimal
//! environment and `cwd = System32`.
#![allow(unsafe_code)]

use std::ffi::{OsStr, c_void};
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::FromRawHandle;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, FILETIME, GetLastError, HANDLE, HANDLE_FLAG_INHERIT, HLOCAL,
    INVALID_HANDLE_VALUE, LocalFree, STILL_ACTIVE, SetHandleInformation, WAIT_OBJECT_0,
    WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSidToSidW, EXPLICIT_ACCESS_W, GRANT_ACCESS,
    GetNamedSecurityInfoW, NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT, SetEntriesInAclW,
    SetNamedSecurityInfoW, TRUSTEE_IS_SID, TRUSTEE_IS_WELL_KNOWN_GROUP, TRUSTEE_W,
};
use windows_sys::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeleteAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
    CONTAINER_INHERIT_ACE, CreateRestrictedToken, DACL_SECURITY_INFORMATION, DISABLE_MAX_PRIVILEGE,
    EqualSid, FreeSid, GetAce, GetAclInformation, GetLengthSid, OBJECT_INHERIT_ACE,
    PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES,
    SUB_CONTAINERS_AND_OBJECTS_INHERIT, SetTokenInformation, TOKEN_ADJUST_DEFAULT,
    TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TokenIntegrityLevel,
};
use windows_sys::Win32::Storage::FileSystem::{FILE_GENERIC_EXECUTE, FILE_GENERIC_READ};
use windows_sys::Win32::System::IO::{
    CreateIoCompletionPort, GetQueuedCompletionStatus, OVERLAPPED,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
    JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_JOB_MEMORY,
    JOB_OBJECT_LIMIT_JOB_TIME, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
    JOBOBJECT_ASSOCIATE_COMPLETION_PORT, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    JOBOBJECT_BASIC_UI_RESTRICTIONS, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectAssociateCompletionPortInformation, JobObjectBasicAccountingInformation,
    JobObjectBasicUIRestrictions, JobObjectExtendedLimitInformation, QueryInformationJobObject,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::SystemServices::SE_GROUP_INTEGRITY;
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, CreateProcessW,
    DETACHED_PROCESS, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
    GetCurrentProcess, GetExitCodeProcess, GetProcessTimes, InitializeProcThreadAttributeList,
    LPPROC_THREAD_ATTRIBUTE_LIST, OpenProcess, OpenProcessToken,
    PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
    PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, ResumeThread,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute,
    WaitForSingleObject,
};

/// Exit code used when the supervisor kills a worker's job.
pub const EXIT_KILLED: u32 = 0xE7E7;

/// SID of "ALL APPLICATION PACKAGES" (every AppContainer token carries it).
const ALL_APP_PACKAGES: &str = "S-1-15-2-1";

// PROCESS_CREATION_MITIGATION_POLICY_* (winnt.h).
const MIT_HEAP_TERMINATE: u64 = 1 << 12;
const MIT_BOTTOM_UP_ASLR: u64 = 1 << 16;
const MIT_STRICT_HANDLE_CHECKS: u64 = 1 << 24;
const MIT_EXTENSION_POINT_DISABLE: u64 = 1 << 32;
const MIT_PROHIBIT_DYNAMIC_CODE: u64 = 1 << 36;
const MIT_IMAGE_NO_REMOTE: u64 = 1 << 52;
const MIT_IMAGE_NO_LOW_LABEL: u64 = 1 << 56;
/// PROCESS_CREATION_CHILD_PROCESS_RESTRICTED.
const CHILD_PROCESS_RESTRICTED: u32 = 1;
/// All JOB_OBJECT_UILIMIT_* bits.
const UI_LIMIT_ALL: u32 = 0xFF;

// JOB_OBJECT_MSG_* (winnt.h).
pub const MSG_END_OF_JOB_TIME: u32 = 1;
pub const MSG_END_OF_PROCESS_TIME: u32 = 2;
pub const MSG_ABNORMAL_EXIT_PROCESS: u32 = 8;
pub const MSG_PROCESS_MEMORY_LIMIT: u32 = 9;
pub const MSG_JOB_MEMORY_LIMIT: u32 = 10;

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(Some(0)).collect()
}

fn last_error(what: &str) -> io::Error {
    let e = io::Error::last_os_error();
    io::Error::new(e.kind(), format!("{what}: {e}"))
}

/// A kernel handle closed on drop.
#[derive(Debug)]
pub struct OwnedHandle(HANDLE);

// SAFETY: kernel handles are process-wide values; the Win32 calls made on them are thread-safe.
unsafe impl Send for OwnedHandle {}
// SAFETY: as above; only immutable Win32 calls are made through `&OwnedHandle`.
unsafe impl Sync for OwnedHandle {}

impl OwnedHandle {
    pub fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            // SAFETY: we own the handle and close it exactly once.
            unsafe { CloseHandle(self.0) };
        }
    }
}

// ------------------------------------------------------------------ Job Object

/// The worker's Job Object: memory caps, CPU backstop, one process, kill on close, UI limits, and
/// a completion port that records why the job ended.
pub struct Job {
    handle: Arc<OwnedHandle>,
    port: OwnedHandle,
    limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
}

/// A shared reference to the job handle that the watchdog (another thread) uses to kill the
/// worker. Sharing the handle (not its value) means it can never outlive the kernel object.
#[derive(Debug, Clone)]
pub struct JobKiller(Arc<OwnedHandle>);

impl JobKiller {
    pub fn kill(&self) {
        // SAFETY: the Arc keeps the handle open.
        unsafe { TerminateJobObject(self.0.raw(), EXIT_KILLED) };
    }
}

impl Job {
    pub fn new(memory_limit: u64, cpu_budget: Duration) -> io::Result<Job> {
        // SAFETY: every out-pointer refers to a live local; the structs are plain data, zeroed
        // and sized with size_of_val; handles are wrapped as soon as they are created.
        unsafe {
            let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if h.is_null() {
                return Err(last_error("CreateJobObject"));
            }
            let handle = Arc::new(OwnedHandle(h));
            let mem = usize::try_from(memory_limit).unwrap_or(usize::MAX);
            let mut li: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            li.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
                | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION
                | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
                | JOB_OBJECT_LIMIT_PROCESS_MEMORY
                | JOB_OBJECT_LIMIT_JOB_MEMORY
                | JOB_OBJECT_LIMIT_JOB_TIME;
            li.BasicLimitInformation.ActiveProcessLimit = 1;
            li.BasicLimitInformation.PerJobUserTimeLimit = cpu_100ns(cpu_budget);
            li.ProcessMemoryLimit = mem;
            li.JobMemoryLimit = mem;
            if SetInformationJobObject(
                h,
                JobObjectExtendedLimitInformation,
                (&li as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of_val(&li) as u32,
            ) == 0
            {
                return Err(last_error("SetInformationJobObject(limits)"));
            }
            let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS {
                UIRestrictionsClass: UI_LIMIT_ALL,
            };
            if SetInformationJobObject(
                h,
                JobObjectBasicUIRestrictions,
                (&ui as *const JOBOBJECT_BASIC_UI_RESTRICTIONS).cast(),
                size_of_val(&ui) as u32,
            ) == 0
            {
                return Err(last_error("SetInformationJobObject(ui)"));
            }
            let p = CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 1);
            if p.is_null() {
                return Err(last_error("CreateIoCompletionPort"));
            }
            let port = OwnedHandle(p);
            let acp = JOBOBJECT_ASSOCIATE_COMPLETION_PORT {
                CompletionKey: h,
                CompletionPort: p,
            };
            if SetInformationJobObject(
                h,
                JobObjectAssociateCompletionPortInformation,
                (&acp as *const JOBOBJECT_ASSOCIATE_COMPLETION_PORT).cast(),
                size_of_val(&acp) as u32,
            ) == 0
            {
                return Err(last_error("SetInformationJobObject(port)"));
            }
            Ok(Job {
                handle,
                port,
                limits: li,
            })
        }
    }

    pub fn killer(&self) -> JobKiller {
        JobKiller(Arc::clone(&self.handle))
    }

    /// Re-arms the CPU backstop for the next request. Setting JOB_TIME again (without
    /// PRESERVE_JOB_TIME) makes the limit relative to the CPU the job has already used.
    pub fn rearm_cpu(&mut self, budget: Duration) {
        self.limits.BasicLimitInformation.PerJobUserTimeLimit = cpu_100ns(budget);
        // SAFETY: valid job handle and a plain struct of the declared size.
        unsafe {
            SetInformationJobObject(
                self.handle.raw(),
                JobObjectExtendedLimitInformation,
                (&self.limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of_val(&self.limits) as u32,
            );
        }
    }

    pub fn terminate(&self) {
        // SAFETY: valid job handle.
        unsafe { TerminateJobObject(self.handle.raw(), EXIT_KILLED) };
    }

    /// Highest commit of the worker since it started (never decreases).
    pub fn peak_process_memory(&self) -> u64 {
        // SAFETY: out-struct is a zeroed local of the declared size.
        unsafe {
            let mut li: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            if QueryInformationJobObject(
                self.handle.raw(),
                JobObjectExtendedLimitInformation,
                (&mut li as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of_val(&li) as u32,
                std::ptr::null_mut(),
            ) == 0
            {
                return 0;
            }
            li.PeakProcessMemoryUsed as u64
        }
    }

    /// User-mode CPU time of the job.
    pub fn user_time(&self) -> Duration {
        // SAFETY: out-struct is a zeroed local of the declared size.
        unsafe {
            let mut a: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = std::mem::zeroed();
            QueryInformationJobObject(
                self.handle.raw(),
                JobObjectBasicAccountingInformation,
                (&mut a as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                size_of_val(&a) as u32,
                std::ptr::null_mut(),
            );
            Duration::from_nanos(
                u64::try_from(a.TotalUserTime)
                    .unwrap_or(0)
                    .saturating_mul(100),
            )
        }
    }

    /// Job messages posted since the last call (non-blocking).
    pub fn drain_messages(&self) -> Vec<u32> {
        let mut v = Vec::new();
        // SAFETY: out-params are locals; zero timeout never blocks.
        unsafe {
            loop {
                let (mut msg, mut key, mut ov) = (0u32, 0usize, std::ptr::null_mut::<OVERLAPPED>());
                if GetQueuedCompletionStatus(self.port.raw(), &mut msg, &mut key, &mut ov, 0) == 0 {
                    break;
                }
                v.push(msg);
                if v.len() > 64 {
                    break;
                }
            }
        }
        v
    }

    fn assign(&self, process: HANDLE) -> io::Result<()> {
        // SAFETY: both handles are valid.
        if unsafe { AssignProcessToJobObject(self.handle.raw(), process) } == 0 {
            return Err(last_error("AssignProcessToJobObject"));
        }
        Ok(())
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // KILL_ON_JOB_CLOSE would do this when the handle closes; be explicit.
        self.terminate();
    }
}

impl std::fmt::Debug for Job {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Job").field("handle", &self.handle).finish()
    }
}

fn cpu_100ns(d: Duration) -> i64 {
    i64::try_from(d.as_nanos() / 100).unwrap_or(i64::MAX)
}

// ------------------------------------------------------------------ AppContainer

/// The extractor's AppContainer profile (zero capabilities). Created once per user; the SID is
/// derived from the name.
#[derive(Debug)]
pub struct AppContainer {
    sid: PSID,
    pub name: String,
}

// SAFETY: the SID buffer is immutable after creation and freed once in Drop.
unsafe impl Send for AppContainer {}
// SAFETY: as above.
unsafe impl Sync for AppContainer {}

impl Drop for AppContainer {
    fn drop(&mut self) {
        if !self.sid.is_null() {
            // SAFETY: SIDs from CreateAppContainerProfile/Derive... are freed with FreeSid.
            unsafe { FreeSid(self.sid) };
        }
    }
}

impl AppContainer {
    /// Creates the profile (no capabilities) or opens the existing one.
    pub fn ensure(name: &str, display: &str) -> io::Result<AppContainer> {
        let n = wide(OsStr::new(name));
        let d = wide(OsStr::new(display));
        let mut sid: PSID = std::ptr::null_mut();
        // SAFETY: strings are NUL-terminated locals; null capability list with count 0.
        let hr = unsafe {
            CreateAppContainerProfile(
                n.as_ptr(),
                d.as_ptr(),
                d.as_ptr(),
                std::ptr::null(),
                0,
                &mut sid,
            )
        };
        if hr == 0 {
            return Ok(AppContainer {
                sid,
                name: name.into(),
            });
        }
        let already = hr == hresult_from_win32(ERROR_ALREADY_EXISTS);
        // SAFETY: as above.
        let hr2 = unsafe { DeriveAppContainerSidFromAppContainerName(n.as_ptr(), &mut sid) };
        if hr2 == 0 {
            return Ok(AppContainer {
                sid,
                name: name.into(),
            });
        }
        // S-04: keep the error kind of the Win32 code (access denied, not supported, …) so the
        // pool can tell "AppContainer unsupported" from a transient failure.
        let decisive = if already { hr2 } else { hr };
        let kind = if (decisive as u32) >> 16 == 0x8007 {
            io::Error::from_raw_os_error((decisive as u32 & 0xFFFF) as i32).kind()
        } else if decisive as u32 == 0x8000_4001 {
            io::ErrorKind::Unsupported // E_NOTIMPL
        } else {
            io::ErrorKind::Other
        };
        Err(io::Error::new(
            kind,
            format!("CreateAppContainerProfile 0x{hr:08X} (exists: {already}), derive 0x{hr2:08X}"),
        ))
    }

    pub fn sid(&self) -> PSID {
        self.sid
    }

    /// The profile SID in `S-1-15-2-…` form (diagnostics, ACL tooling).
    pub fn sid_string(&self) -> io::Result<String> {
        let mut out: *mut u16 = std::ptr::null_mut();
        // SAFETY: valid SID; the returned string is freed with LocalFree.
        unsafe {
            if ConvertSidToStringSidW(self.sid, &mut out) == 0 {
                return Err(last_error("ConvertSidToStringSid"));
            }
            let _g = LocalGuard(out as HLOCAL);
            let mut n = 0usize;
            while *out.add(n) != 0 {
                n += 1;
            }
            Ok(String::from_utf16_lossy(std::slice::from_raw_parts(out, n)))
        }
    }
}

/// Deletes the extractor's AppContainer profile (uninstall, Amendment 3.17). Missing → Ok.
pub fn delete_app_container_profile(name: &str) -> io::Result<()> {
    let n = wide(OsStr::new(name));
    // SAFETY: NUL-terminated local string.
    let hr = unsafe { DeleteAppContainerProfile(n.as_ptr()) };
    if hr == 0 || hr == hresult_from_win32(2) {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "DeleteAppContainerProfile 0x{hr:08X}"
        )))
    }
}

fn hresult_from_win32(code: u32) -> i32 {
    if code == 0 {
        0
    } else {
        ((code & 0xFFFF) | 0x8007_0000) as i32
    }
}

/// Ensures "ALL APPLICATION PACKAGES" has inheritable read+execute on `dir` (the layout Program
/// Files already has). Returns whether the DACL was changed. Only an allow ACE is ever added; the
/// existing ACEs, owner and label are untouched.
pub fn ensure_app_packages_read(dir: &Path) -> io::Result<bool> {
    let path = wide(dir.as_os_str());
    let sddl = wide(OsStr::new(ALL_APP_PACKAGES));
    let want = FILE_GENERIC_READ | FILE_GENERIC_EXECUTE;
    // SAFETY: SID and SD buffers are freed with LocalFree; the ACL pointer points into the SD;
    // ACE pointers come from GetAce on that ACL and are only read.
    unsafe {
        let mut sid: PSID = std::ptr::null_mut();
        if ConvertStringSidToSidW(sddl.as_ptr(), &mut sid) == 0 {
            return Err(last_error("ConvertStringSidToSid"));
        }
        let _sid_guard = LocalGuard(sid as HLOCAL);
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        let e = GetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut sd,
        );
        if e != 0 {
            return Err(io::Error::from_raw_os_error(e as i32));
        }
        let _sd_guard = LocalGuard(sd as HLOCAL);
        if !dacl.is_null() && acl_grants(dacl, sid, want) {
            return Ok(false);
        }
        let ea = EXPLICIT_ACCESS_W {
            grfAccessPermissions: want,
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: SUB_CONTAINERS_AND_OBJECTS_INHERIT,
            Trustee: TRUSTEE_W {
                pMultipleTrustee: std::ptr::null_mut(),
                MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_WELL_KNOWN_GROUP,
                ptstrName: sid as *mut u16,
            },
        };
        let mut new_acl: *mut ACL = std::ptr::null_mut();
        let e = SetEntriesInAclW(1, &ea, dacl, &mut new_acl);
        if e != 0 {
            return Err(io::Error::from_raw_os_error(e as i32));
        }
        let _acl_guard = LocalGuard(new_acl as HLOCAL);
        let mut p = path.clone();
        let e = SetNamedSecurityInfoW(
            p.as_mut_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            new_acl,
            std::ptr::null(),
        );
        if e != 0 {
            return Err(io::Error::from_raw_os_error(e as i32));
        }
        Ok(true)
    }
}

/// Whether `acl` has an inheritable (OI+CI) allow ACE for `sid` covering `want`.
///
/// # Safety
/// `acl` must point to a valid ACL and `sid` to a valid SID.
unsafe fn acl_grants(acl: *const ACL, sid: PSID, want: u32) -> bool {
    // SAFETY: guaranteed by the caller; ACE pointers come from GetAce.
    unsafe {
        let mut info: ACL_SIZE_INFORMATION = std::mem::zeroed();
        if GetAclInformation(
            acl,
            (&mut info as *mut ACL_SIZE_INFORMATION).cast(),
            size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        ) == 0
        {
            return false;
        }
        for i in 0..info.AceCount {
            let mut ace: *mut c_void = std::ptr::null_mut();
            if GetAce(acl, i, &mut ace) == 0 {
                continue;
            }
            let hdr = &*(ace as *const ACE_HEADER);
            // ACCESS_ALLOWED_ACE_TYPE = 0
            if hdr.AceType != 0 {
                continue;
            }
            let inherit = u32::from(hdr.AceFlags) & (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE);
            if inherit != (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE) {
                continue;
            }
            let allowed = &*(ace as *const ACCESS_ALLOWED_ACE);
            let ace_sid = (&allowed.SidStart as *const u32) as PSID;
            if EqualSid(ace_sid, sid) != 0 && allowed.Mask & want == want {
                return true;
            }
        }
        false
    }
}

struct LocalGuard(HLOCAL);

impl Drop for LocalGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: allocated by a Win32 API documented to be freed with LocalFree.
            unsafe { LocalFree(self.0) };
        }
    }
}

// ------------------------------------------------------------------ process

/// Token used for the worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    AppContainer,
    LowIntegrity,
}

/// A running worker: its process handle, pid and the parent's ends of the three pipes.
#[derive(Debug)]
pub struct ChildProcess {
    pub process: OwnedHandle,
    pub pid: u32,
    pub stdin: File,
    pub stdout: File,
    pub stderr: File,
}

impl ChildProcess {
    /// Waits up to `timeout` for exit; returns the exit code if it exited.
    pub fn wait_exit(&self, timeout: Duration) -> Option<u32> {
        let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        // SAFETY: valid process handle; out-param is a local.
        unsafe {
            match WaitForSingleObject(self.process.raw(), ms) {
                WAIT_OBJECT_0 => {
                    let mut code = 0u32;
                    GetExitCodeProcess(self.process.raw(), &mut code);
                    Some(code)
                }
                _ => None,
            }
        }
    }
}

fn pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
    let sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    };
    let (mut r, mut w) = (std::ptr::null_mut(), std::ptr::null_mut());
    // SAFETY: out-params are locals; 1 MiB buffer hint.
    if unsafe { CreatePipe(&mut r, &mut w, &sa, 1 << 20) } == 0 {
        return Err(last_error("CreatePipe"));
    }
    Ok((OwnedHandle(r), OwnedHandle(w)))
}

fn not_inheritable(h: &OwnedHandle) {
    // SAFETY: valid handle.
    unsafe { SetHandleInformation(h.raw(), HANDLE_FLAG_INHERIT, 0) };
}

/// CreateRestrictedToken(DISABLE_MAX_PRIVILEGE) at Low integrity.
fn low_il_token() -> io::Result<OwnedHandle> {
    let sddl = wide(OsStr::new("S-1-16-4096"));
    // SAFETY: handles are wrapped immediately; the SID is freed with LocalFree; the label struct
    // points at the SID for the duration of the call only.
    unsafe {
        let mut tok = std::ptr::null_mut();
        if OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ADJUST_DEFAULT | TOKEN_ASSIGN_PRIMARY,
            &mut tok,
        ) == 0
        {
            return Err(last_error("OpenProcessToken"));
        }
        let tok = OwnedHandle(tok);
        let mut rt = std::ptr::null_mut();
        if CreateRestrictedToken(
            tok.raw(),
            DISABLE_MAX_PRIVILEGE,
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
            &mut rt,
        ) == 0
        {
            return Err(last_error("CreateRestrictedToken"));
        }
        let rt = OwnedHandle(rt);
        let mut sid: PSID = std::ptr::null_mut();
        if ConvertStringSidToSidW(sddl.as_ptr(), &mut sid) == 0 {
            return Err(last_error("ConvertStringSidToSid"));
        }
        let _g = LocalGuard(sid as HLOCAL);
        let tml = TOKEN_MANDATORY_LABEL {
            Label: SID_AND_ATTRIBUTES {
                Sid: sid,
                Attributes: SE_GROUP_INTEGRITY as u32,
            },
        };
        if SetTokenInformation(
            rt.raw(),
            TokenIntegrityLevel,
            (&tml as *const TOKEN_MANDATORY_LABEL).cast(),
            size_of::<TOKEN_MANDATORY_LABEL>() as u32 + GetLengthSid(sid),
        ) == 0
        {
            return Err(last_error("SetTokenInformation(integrity)"));
        }
        Ok(rt)
    }
}

/// Minimal environment block. AppContainer creation fails with ERROR_ENVVAR_NOT_FOUND (203)
/// unless the profile variables are present (M0-C).
fn environment(kind: TokenKind) -> Vec<u16> {
    let sysroot = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    let mut env = format!("SystemRoot={sysroot}\0");
    if kind == TokenKind::AppContainer {
        for k in [
            "USERPROFILE",
            "LOCALAPPDATA",
            "APPDATA",
            "TEMP",
            "TMP",
            "HOMEDRIVE",
            "HOMEPATH",
            "USERNAME",
        ] {
            if let Ok(v) = std::env::var(k) {
                env.push_str(&format!("{k}={v}\0"));
            }
        }
    }
    env.push('\0');
    env.encode_utf16().collect()
}

/// Starts `exe args…` suspended, assigns it to `job`, then resumes it.
pub fn spawn(
    exe: &Path,
    args: &[&OsStr],
    kind: TokenKind,
    app_container: Option<&AppContainer>,
    job: &Job,
) -> io::Result<ChildProcess> {
    let (in_r, in_w) = pipe()?;
    let (out_r, out_w) = pipe()?;
    let (err_r, err_w) = pipe()?;
    for h in [&in_w, &out_r, &err_r] {
        not_inheritable(h);
    }
    let handles = [in_r.raw(), out_w.raw(), err_w.raw()];
    let mitigation: u64 = MIT_HEAP_TERMINATE
        | MIT_BOTTOM_UP_ASLR
        | MIT_STRICT_HANDLE_CHECKS
        | MIT_EXTENSION_POINT_DISABLE
        | MIT_PROHIBIT_DYNAMIC_CODE
        | MIT_IMAGE_NO_REMOTE
        | MIT_IMAGE_NO_LOW_LABEL;
    let child_policy: u32 = CHILD_PROCESS_RESTRICTED;

    let mut cmd = String::new();
    cmd.push('"');
    cmd.push_str(&exe.to_string_lossy());
    cmd.push('"');
    for a in args {
        cmd.push_str(" \"");
        cmd.push_str(&a.to_string_lossy());
        cmd.push('"');
    }
    let mut cmd: Vec<u16> = cmd.encode_utf16().chain(Some(0)).collect();
    let app = wide(exe.as_os_str());
    let env = environment(kind);
    let sysroot = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    let cwd = wide(OsStr::new(&format!(r"{sysroot}\System32")));
    let token = match kind {
        TokenKind::LowIntegrity => Some(low_il_token()?),
        TokenKind::AppContainer => None,
    };

    // SAFETY: the attribute list buffer outlives every use (deleted before return); all values
    // passed to UpdateProcThreadAttribute (handles, policies, capabilities) are locals that live
    // until CreateProcess returns; the env/cmd/app/cwd buffers are NUL-terminated locals.
    unsafe {
        let mut sc: SECURITY_CAPABILITIES = std::mem::zeroed();
        let mut n_attr = 3u32;
        if kind == TokenKind::AppContainer {
            let ac = app_container
                .ok_or_else(|| io::Error::other("AppContainer token without a profile"))?;
            sc.AppContainerSid = ac.sid();
            n_attr += 1;
        }
        let mut size = 0usize;
        InitializeProcThreadAttributeList(std::ptr::null_mut(), n_attr, 0, &mut size);
        let mut attr_buf = vec![0u64; size.div_ceil(8)];
        let attrs: LPPROC_THREAD_ATTRIBUTE_LIST = attr_buf.as_mut_ptr().cast();
        if InitializeProcThreadAttributeList(attrs, n_attr, 0, &mut size) == 0 {
            return Err(last_error("InitializeProcThreadAttributeList"));
        }
        struct AttrGuard(LPPROC_THREAD_ATTRIBUTE_LIST);
        impl Drop for AttrGuard {
            fn drop(&mut self) {
                // SAFETY: initialised above, deleted once.
                unsafe { DeleteProcThreadAttributeList(self.0) };
            }
        }
        let _ag = AttrGuard(attrs);
        let upd = |a: usize, v: *const c_void, n: usize, what: &str| -> io::Result<()> {
            // Inside the enclosing unsafe block; see its SAFETY comment.
            if UpdateProcThreadAttribute(attrs, 0, a, v, n, std::ptr::null_mut(), std::ptr::null())
                == 0
            {
                Err(last_error(what))
            } else {
                Ok(())
            }
        };
        upd(
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            handles.as_ptr().cast(),
            size_of_val(&handles),
            "attribute(handles)",
        )?;
        upd(
            PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY as usize,
            (&child_policy as *const u32).cast(),
            size_of::<u32>(),
            "attribute(child policy)",
        )?;
        upd(
            PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY as usize,
            (&mitigation as *const u64).cast(),
            size_of::<u64>(),
            "attribute(mitigations)",
        )?;
        if kind == TokenKind::AppContainer {
            upd(
                PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
                (&sc as *const SECURITY_CAPABILITIES).cast(),
                size_of_val(&sc),
                "attribute(appcontainer)",
            )?;
        }

        let mut si: STARTUPINFOEXW = std::mem::zeroed();
        si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = in_r.raw();
        si.StartupInfo.hStdOutput = out_w.raw();
        si.StartupInfo.hStdError = err_w.raw();
        si.lpAttributeList = attrs;
        // DETACHED_PROCESS, never CREATE_NO_WINDOW (see the module docs).
        let flags = EXTENDED_STARTUPINFO_PRESENT
            | CREATE_SUSPENDED
            | DETACHED_PROCESS
            | CREATE_UNICODE_ENVIRONMENT;
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        let ok = match &token {
            Some(t) => CreateProcessAsUserW(
                t.raw(),
                app.as_ptr(),
                cmd.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                flags,
                env.as_ptr().cast(),
                cwd.as_ptr(),
                &si.StartupInfo,
                &mut pi,
            ),
            None => CreateProcessW(
                app.as_ptr(),
                cmd.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                flags,
                env.as_ptr().cast(),
                cwd.as_ptr(),
                &si.StartupInfo,
                &mut pi,
            ),
        };
        if ok == 0 {
            return Err(last_error(match kind {
                TokenKind::AppContainer => "CreateProcess(AppContainer)",
                TokenKind::LowIntegrity => "CreateProcessAsUser(low IL)",
            }));
        }
        let process = OwnedHandle(pi.hProcess);
        let thread = OwnedHandle(pi.hThread);
        if let Err(e) = job.assign(process.raw()) {
            TerminateProcess(process.raw(), EXIT_KILLED);
            return Err(e);
        }
        if ResumeThread(thread.raw()) == u32::MAX {
            let e = last_error("ResumeThread");
            job.terminate();
            return Err(e);
        }
        drop(thread);
        // Our copies of the child's ends must close, or EOF never arrives when it dies.
        drop((in_r, out_w, err_w));
        let into_file = |h: OwnedHandle| {
            let raw = h.0;
            std::mem::forget(h);
            File::from_raw_handle(raw)
        };
        Ok(ChildProcess {
            process,
            pid: pi.dwProcessId,
            stdin: into_file(in_w),
            stdout: into_file(out_r),
            stderr: into_file(err_r),
        })
    }
}

/// A worker process identity that survives pid reuse: pid plus creation time (100 ns FILETIME).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WorkerId {
    pub pid: u32,
    pub created: u64,
}

impl WorkerId {
    /// Identity of the process behind `h`.
    pub fn of(h: &OwnedHandle, pid: u32) -> WorkerId {
        WorkerId {
            pid,
            created: creation_time(h.raw()).unwrap_or(0),
        }
    }

    /// Whether this exact process (same pid **and** creation time) is still running. A recycled
    /// pid that now belongs to another process reports `false`.
    pub fn is_alive(&self) -> bool {
        // SAFETY: the handle is checked and closed; out-params are locals.
        unsafe {
            let h = OpenProcess(
                PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                self.pid,
            );
            if h.is_null() {
                return false;
            }
            let h = OwnedHandle(h);
            if creation_time(h.raw()) != Some(self.created) {
                return false;
            }
            let mut code = 0u32;
            WaitForSingleObject(h.raw(), 0) == WAIT_TIMEOUT
                && GetExitCodeProcess(h.raw(), &mut code) != 0
                && code == STILL_ACTIVE as u32
        }
    }
}

fn creation_time(h: HANDLE) -> Option<u64> {
    let z = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut c, mut e, mut k, mut u) = (z, z, z, z);
    // SAFETY: valid process handle; out-params are locals.
    if unsafe { GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u) } == 0 {
        return None;
    }
    Some((u64::from(c.dwHighDateTime) << 32) | u64::from(c.dwLowDateTime))
}

/// Last Win32 error code (diagnostics).
pub fn last_error_code() -> u32 {
    // SAFETY: no preconditions.
    unsafe { GetLastError() }
}
