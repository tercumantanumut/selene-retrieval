//! Win32 FFI used by the pipe client, the launcher and the daemon's pipe server.
//!
//! This is the only module of sr-client that may use `unsafe` (workspace lint `unsafe_code =
//! "deny"`, allowed here for reviewed Win32 calls). Every function wraps one API and returns
//! `io::Result`; handles are owned by RAII types.

use std::ffi::{OsStr, c_void};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, RawHandle};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_LOCK_VIOLATION, HANDLE, LocalFree, STILL_ACTIVE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertSidToStringSidW,
    ConvertStringSecurityDescriptorToSecurityDescriptorW, ConvertStringSidToSidW, GetSecurityInfo,
    SDDL_REVISION_1, SE_FILE_OBJECT, SE_KERNEL_OBJECT, SE_OBJECT_TYPE, SetNamedSecurityInfoW,
};
use windows_sys::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetSecurityDescriptorOwner,
    GetTokenInformation, LABEL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES,
    TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, LOCKFILE_EXCLUSIVE_LOCK,
    LOCKFILE_FAIL_IMMEDIATELY, LockFileEx, READ_CONTROL,
};
use windows_sys::Win32::Storage::Packaging::Appx::GetCurrentPackageFullName;
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::IO::OVERLAPPED;
use windows_sys::Win32::System::JobObjects::IsProcessInJob;
use windows_sys::Win32::System::Pipes::{GetNamedPipeClientProcessId, GetNamedPipeServerProcessId};
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, GetCurrentProcess,
    GetExitCodeProcess, OpenProcess, OpenProcessToken, PROCESS_INFORMATION, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW, ResumeThread, STARTUPINFOW,
    TerminateProcess, WaitForSingleObject,
};
use windows_sys::Win32::UI::Shell::{
    FOLDERID_LocalAppData, KF_FLAG_DONT_VERIFY, SHGetKnownFolderPath,
};

/// `ERROR_FILE_NOT_FOUND`: no pipe with that name.
pub const ERROR_FILE_NOT_FOUND: i32 = 2;
/// `ERROR_ACCESS_DENIED`.
pub const ERROR_ACCESS_DENIED: i32 = 5;
/// `ERROR_PIPE_BUSY`: the pipe exists but every instance is taken.
pub const ERROR_PIPE_BUSY: i32 = 231;
/// `APPMODEL_ERROR_NO_PACKAGE`.
const APPMODEL_ERROR_NO_PACKAGE: u32 = 15700;

/// NUL-terminated UTF-16.
pub fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

/// Reads a NUL-terminated UTF-16 string.
///
/// # Safety
/// `p` must point to a NUL-terminated UTF-16 buffer.
unsafe fn from_wide_ptr(p: *const u16) -> String {
    let mut n = 0;
    // SAFETY: caller guarantees NUL termination.
    while unsafe { *p.add(n) } != 0 {
        n += 1;
    }
    // SAFETY: `n` elements were just read.
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, n) })
}

fn raw(h: &impl AsRawHandle) -> HANDLE {
    h.as_raw_handle().cast()
}

/// String SID of the current process token user ("S-1-5-21-...-1001").
pub fn user_sid_string() -> io::Result<String> {
    // SAFETY: standard token query; every handle/buffer is checked and freed.
    unsafe {
        let mut tok: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok) == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buf = vec![0u64; 64]; // 512 bytes, 8-aligned for TOKEN_USER
        let mut len = 0u32;
        let ok = GetTokenInformation(tok, TokenUser, buf.as_mut_ptr().cast(), 512, &mut len);
        CloseHandle(tok);
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let tu = &*(buf.as_ptr().cast::<TOKEN_USER>());
        let mut s: *mut u16 = std::ptr::null_mut();
        if ConvertSidToStringSidW(tu.User.Sid, &mut s) == 0 {
            return Err(io::Error::last_os_error());
        }
        let out = from_wide_ptr(s);
        LocalFree(s.cast());
        Ok(out)
    }
}

/// Amendment 13: whether this process runs elevated ("Run as administrator";
/// `GetTokenInformation(TokenElevation)`). The app refuses to make the default folder then, so
/// the folder never belongs to the Administrators group.
pub fn process_elevated() -> io::Result<bool> {
    use windows_sys::Win32::Security::{TOKEN_ELEVATION, TokenElevation};
    let mut e = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let size = u32::try_from(size_of::<TOKEN_ELEVATION>()).map_err(io::Error::other)?;
    // SAFETY: the token handle is checked and closed once; `e` is a caller-owned TOKEN_ELEVATION
    // whose exact size is passed.
    unsafe {
        let mut tok: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok) == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut len = 0u32;
        let ok = GetTokenInformation(tok, TokenElevation, (&raw mut e).cast(), size, &mut len);
        CloseHandle(tok);
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(e.TokenIsElevated != 0)
}

/// M5-E S-08: `SetDefaultDllDirectories(SYSTEM32 | APPLICATION_DIR)`, so no later by-name
/// `LoadLibrary` (Rust std, delay-loads, WebView2 loader, …) searches the current directory or
/// `PATH`. Call it as the **first statement of every `main`** (sr-app, sr-bridge, sr-launch,
/// sr-setup; sr-daemon calls the DirectML-aware `sr_providers::dml::harden_dll_search`, which
/// does the same plus a System32 `dxgi.dll` map). Idempotent; returns false if the call failed.
pub fn harden_dll_search() -> bool {
    use windows_sys::Win32::System::LibraryLoader::{
        LOAD_LIBRARY_SEARCH_APPLICATION_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32, SetDefaultDllDirectories,
    };
    // SAFETY: plain flag call; affects later LoadLibrary calls of this process only.
    unsafe {
        SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32 | LOAD_LIBRARY_SEARCH_APPLICATION_DIR)
            != 0
    }
}

/// M5-E S-03: `SO_EXCLUSIVEADDRUSE` on a TCP socket **before** `bind`, so no other process (same
/// user included) can bind the same address next to ours with `SO_REUSEADDR`.
pub fn set_exclusive_addr_use(sock: &impl std::os::windows::io::AsRawSocket) -> io::Result<()> {
    use windows_sys::Win32::Networking::WinSock::{
        SO_EXCLUSIVEADDRUSE, SOCKET, SOL_SOCKET, WSAGetLastError, setsockopt,
    };
    let on: i32 = 1;
    // SAFETY: `sock` is a live socket owned by the caller; `on` outlives the call and its size
    // is passed exactly.
    let rc = unsafe {
        setsockopt(
            sock.as_raw_socket() as SOCKET,
            SOL_SOCKET,
            SO_EXCLUSIVEADDRUSE,
            (&raw const on).cast(),
            std::mem::size_of::<i32>() as i32,
        )
    };
    if rc != 0 {
        // SAFETY: no arguments; reads this thread's last Winsock error.
        return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
    }
    Ok(())
}

/// M5-E S-02: a SID string or SDDL alias ("BA", "SY", "LA", …) in the canonical `S-1-…` form,
/// so an owner printed as an alias compares equal to [`user_sid_string`].
pub fn canonical_sid(sid_or_alias: &str) -> io::Result<String> {
    let w = wide(sid_or_alias);
    // SAFETY: `sid` is LocalAlloc'd by the first conversion and freed once; `s` likewise.
    unsafe {
        let mut sid: PSID = std::ptr::null_mut();
        if ConvertStringSidToSidW(w.as_ptr(), &mut sid) == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut s: *mut u16 = std::ptr::null_mut();
        let ok = ConvertSidToStringSidW(sid, &mut s);
        LocalFree(sid.cast());
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let out = from_wide_ptr(s);
        LocalFree(s.cast());
        Ok(out)
    }
}

/// `%LOCALAPPDATA%` from the known-folder API (never from the environment: MCP clients pass a
/// filtered environment, and a task-launched daemon gets the task's).
pub fn local_appdata() -> io::Result<PathBuf> {
    // SAFETY: the returned buffer is freed with CoTaskMemFree.
    unsafe {
        let mut p: *mut u16 = std::ptr::null_mut();
        let hr = SHGetKnownFolderPath(
            &FOLDERID_LocalAppData,
            KF_FLAG_DONT_VERIFY as _,
            std::ptr::null_mut(),
            &mut p,
        );
        if hr != 0 || p.is_null() {
            if !p.is_null() {
                CoTaskMemFree(p.cast());
            }
            return Err(io::Error::other(format!(
                "SHGetKnownFolderPath hr=0x{hr:x}"
            )));
        }
        let s = from_wide_ptr(p);
        CoTaskMemFree(p.cast());
        Ok(PathBuf::from(s))
    }
}

/// `C:\Windows\System32` from `GetSystemDirectoryW` (Claude passes no WINDIR/SystemRoot).
pub fn system_dir() -> io::Result<PathBuf> {
    let mut buf = vec![0u16; 520];
    // SAFETY: buffer length is passed.
    let n = unsafe { GetSystemDirectoryW(buf.as_mut_ptr(), 520) } as usize;
    if n == 0 || n >= buf.len() {
        return Err(io::Error::last_os_error());
    }
    Ok(PathBuf::from(String::from_utf16_lossy(&buf[..n])))
}

/// Opens (creating) `path` and takes an exclusive `LockFileEx` lock on its first byte.
/// `wait=false` returns `Ok(None)` when another process holds the lock.
/// The lock lives as long as the returned `File`.
pub fn lock_file(path: &Path, wait: bool) -> io::Result<Option<File>> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    let mut flags = LOCKFILE_EXCLUSIVE_LOCK;
    if !wait {
        flags |= LOCKFILE_FAIL_IMMEDIATELY;
    }
    // SAFETY: the OVERLAPPED lives across the synchronous call; the handle is valid.
    unsafe {
        let mut ov: OVERLAPPED = std::mem::zeroed();
        if LockFileEx(raw(&f), flags, 0, 1, 0, &mut ov) == 0 {
            let e = io::Error::last_os_error();
            if !wait && e.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
                return Ok(None);
            }
            return Err(e);
        }
    }
    Ok(Some(f))
}

/// `Some(full name)` when the current process has package identity (MSIX), else `None`.
pub fn package_full_name() -> Option<String> {
    // SAFETY: length query then a buffer of that length.
    unsafe {
        let mut len = 0u32;
        let rc = GetCurrentPackageFullName(&mut len, std::ptr::null_mut());
        if rc == APPMODEL_ERROR_NO_PACKAGE {
            return None;
        }
        let mut buf = vec![0u16; len as usize + 1];
        if GetCurrentPackageFullName(&mut len, buf.as_mut_ptr()) != 0 {
            return Some(format!("<package, error {rc}>"));
        }
        Some(String::from_utf16_lossy(
            &buf[..(len as usize).saturating_sub(1)],
        ))
    }
}

/// Whether the current process is in any Job Object.
pub fn current_process_in_job() -> Option<bool> {
    let mut r = 0;
    // SAFETY: pseudo-handle of the current process.
    let ok = unsafe { IsProcessInJob(GetCurrentProcess(), std::ptr::null_mut(), &mut r) };
    (ok != 0).then_some(r != 0)
}

/// Whether process `pid` is still running.
pub fn pid_alive(pid: u32) -> bool {
    // SAFETY: handle checked and closed.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return false;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(h, &mut code);
        CloseHandle(h);
        ok != 0 && code == STILL_ACTIVE as u32
    }
}

/// Full image path of process `pid` (`QueryFullProcessImageNameW`, Win32 form), `None` when the
/// process is gone or can't be queried. M4-E (R31): the daemon checks that privileged app-RPC
/// callers are its own `sr-app.exe` / `sr-daemon.exe`.
pub fn process_image_path(pid: u32) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    // 32 767 UTF-16 units is the longest Win32 path.
    let mut buf = vec![0u16; 32_768];
    let mut len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
    // SAFETY: the handle is checked and closed; `buf` outlives the call and `len` is its length
    // in UTF-16 units; the API writes at most `len` units and updates `len` to the count written.
    let ok = unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len);
        CloseHandle(h);
        ok
    };
    if ok == 0 {
        return None;
    }
    buf.truncate(len as usize);
    Some(PathBuf::from(std::ffi::OsString::from_wide(&buf)))
}

/// Windows command-line quoting for one argument (CommandLineToArgvW rules).
pub fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_string();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            c => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

/// A child process created with `bInheritHandles = FALSE`.
///
/// `std::process::Command` always passes `TRUE`, which would leak the MCP client's stdio pipe
/// handles into the daemon and keep the client's pipes open after the bridge dies.
#[derive(Debug)]
pub struct OwnedProcess {
    process: isize,
    thread: isize,
    pid: u32,
}

impl OwnedProcess {
    /// `CreateProcessW(app, "<app> <args>", inherit=FALSE, flags)`. With `suspended`, the main
    /// thread is created suspended and must be [`resume`](Self::resume)d.
    pub fn spawn(
        app: &Path,
        args: &str,
        flags: u32,
        suspended: bool,
        cwd: Option<&Path>,
    ) -> io::Result<Self> {
        let app_w = wide(app);
        let mut cmd = quote_arg(&app.to_string_lossy());
        if !args.is_empty() {
            cmd.push(' ');
            cmd.push_str(args);
        }
        let mut cmd_w = wide(&cmd);
        let cwd_w = cwd.map(wide);
        let flags =
            flags | CREATE_UNICODE_ENVIRONMENT | if suspended { CREATE_SUSPENDED } else { 0 };
        // SAFETY: all buffers outlive the call; handles are owned by the returned value.
        unsafe {
            let mut si: STARTUPINFOW = std::mem::zeroed();
            si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
            let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
            let ok = CreateProcessW(
                app_w.as_ptr(),
                cmd_w.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                flags,
                std::ptr::null(),
                cwd_w.as_ref().map_or(std::ptr::null(), |w| w.as_ptr()),
                &si,
                &mut pi,
            );
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self {
                process: pi.hProcess as isize,
                thread: pi.hThread as isize,
                pid: pi.dwProcessId,
            })
        }
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    fn h(&self) -> HANDLE {
        self.process as HANDLE
    }

    /// `IsProcessInJob(child, NULL)`: is the child in *any* job?
    pub fn in_any_job(&self) -> io::Result<bool> {
        let mut r = 0;
        // SAFETY: owned process handle.
        if unsafe { IsProcessInJob(self.h(), std::ptr::null_mut(), &mut r) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(r != 0)
    }

    /// Resumes the main thread of a suspended child.
    pub fn resume(&self) -> io::Result<()> {
        // SAFETY: owned thread handle.
        if unsafe { ResumeThread(self.thread as HANDLE) } == u32::MAX {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn terminate(&self, code: u32) -> io::Result<()> {
        // SAFETY: owned process handle.
        if unsafe { TerminateProcess(self.h(), code) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Exit code if the process has exited within `timeout`, else `None`.
    pub fn wait(&self, timeout: Duration) -> io::Result<Option<u32>> {
        let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
        // SAFETY: owned process handle.
        unsafe {
            match WaitForSingleObject(self.h(), ms) {
                WAIT_OBJECT_0 => {
                    let mut code = 0u32;
                    if GetExitCodeProcess(self.h(), &mut code) == 0 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(Some(code))
                }
                WAIT_TIMEOUT => Ok(None),
                _ => Err(io::Error::last_os_error()),
            }
        }
    }

    /// Non-blocking poll of the exit code.
    pub fn try_wait(&self) -> io::Result<Option<u32>> {
        self.wait(Duration::ZERO)
    }

    /// Async wait (polls every 10 ms; never blocks the runtime).
    pub async fn wait_async(&self, timeout: Duration) -> io::Result<Option<u32>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(c) = self.try_wait()? {
                return Ok(Some(c));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

impl Drop for OwnedProcess {
    fn drop(&mut self) {
        // SAFETY: owned handles, closed once.
        unsafe {
            if self.thread != 0 {
                CloseHandle(self.thread as HANDLE);
            }
            if self.process != 0 {
                CloseHandle(self.process as HANDLE);
            }
        }
    }
}

/// Security descriptor of the daemon's pipe:
/// `D:P(A;;GA;;;<user SID>)S:(ML;;NWNR;;;ME)` — protected DACL with one ACE for the current user,
/// plus an explicit Medium mandatory label with no-write-up and no-read-up, so a Low-IL process of
/// the same user cannot open an instance even read-only (M0-D §3).
pub struct PipeSecurity {
    sd: PSECURITY_DESCRIPTOR,
    sddl: String,
}

// SAFETY: the descriptor is immutable after creation and only read by CreateNamedPipeW.
unsafe impl Send for PipeSecurity {}
// SAFETY: see above.
unsafe impl Sync for PipeSecurity {}

impl PipeSecurity {
    /// Descriptor for the current user.
    pub fn current_user() -> io::Result<Self> {
        Self::for_sid(&user_sid_string()?)
    }

    pub fn for_sid(sid: &str) -> io::Result<Self> {
        if !sid.starts_with("S-1-") || !sid.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "bad SID"));
        }
        let sddl = format!("D:P(A;;GA;;;{sid})S:(ML;;NWNR;;;ME)");
        Self::from_sddl(&sddl)
    }

    /// Arbitrary SDDL (tests use this for control pipes).
    pub fn from_sddl(sddl: &str) -> io::Result<Self> {
        let w = wide(sddl);
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: output descriptor is LocalAlloc'd and freed in Drop.
        unsafe {
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                w.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                std::ptr::null_mut(),
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Self {
            sd,
            sddl: sddl.to_string(),
        })
    }

    pub fn sddl(&self) -> &str {
        &self.sddl
    }

    /// Creates one pipe instance: duplex byte mode, `PIPE_REJECT_REMOTE_CLIENTS`, and
    /// `FILE_FLAG_FIRST_PIPE_INSTANCE` when `first` (a squatter that created the name first makes
    /// this fail with access denied instead of silently sharing the name).
    pub fn create_server(&self, name: &str, first: bool) -> io::Result<NamedPipeServer> {
        let mut sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.sd,
            bInheritHandle: 0,
        };
        // SAFETY: `sa` and the descriptor outlive the call; tokio copies nothing from them later.
        unsafe {
            ServerOptions::new()
                .first_pipe_instance(first)
                .reject_remote_clients(true)
                .max_instances(254)
                .create_with_security_attributes_raw(
                    name,
                    (&mut sa as *mut SECURITY_ATTRIBUTES).cast::<c_void>(),
                )
        }
    }
}

impl Drop for PipeSecurity {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe {
            LocalFree(self.sd.cast());
        }
    }
}

/// Process id of the client connected to a server pipe instance.
pub fn pipe_client_pid(pipe: &impl AsRawHandle) -> Option<u32> {
    let mut pid = 0u32;
    // SAFETY: valid pipe handle.
    let ok = unsafe { GetNamedPipeClientProcessId(raw(pipe), &mut pid) };
    (ok != 0).then_some(pid)
}

/// Well-known `BUILTIN\Administrators`: the default owner of objects created by an elevated
/// process of an administrator. Accepted as the pipe **owner** only together with a server
/// process whose token user is ours (another account can't set either without SeRestore).
pub const ADMINISTRATORS_SID: &str = "S-1-5-32-544";

/// Why a pipe server was refused (diagnostics only; the user sees one plain sentence).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerIdentity {
    /// Owner and server process user are the expected SID.
    Ours,
    /// The pipe's owner SID is someone else.
    ForeignOwner(String),
    /// The server process runs as someone else.
    ForeignProcess(String),
    /// The server process (or its token) could not be opened: a process of another account
    /// at the same integrity level denies `PROCESS_QUERY_LIMITED_INFORMATION`. Fail closed.
    Unverifiable(String),
}

/// The pure decision of [`pipe_server_identity`] (unit-tested without a second account).
///
/// Rules, in order:
/// 1. An owner that is neither us nor `BUILTIN\Administrators` → `ForeignOwner` (deterministic:
///    another account can't make us the owner without SeRestore/SeTakeOwnership).
/// 2. A readable server-process user that isn't us → `ForeignProcess`.
/// 3. Process user unreadable (process gone mid-drain, or an elevated server): accept only when
///    the owner is exactly us; with the Administrators owner it is `Unverifiable`.
pub fn decide_server_identity(
    expected: &str,
    owner: &str,
    process_user: Option<&str>,
) -> ServerIdentity {
    let owner_is_us = owner.eq_ignore_ascii_case(expected);
    if !(owner_is_us || owner.eq_ignore_ascii_case(ADMINISTRATORS_SID)) {
        return ServerIdentity::ForeignOwner(owner.to_string());
    }
    match process_user {
        Some(user) if !user.eq_ignore_ascii_case(expected) => {
            ServerIdentity::ForeignProcess(user.to_string())
        }
        Some(_) => ServerIdentity::Ours,
        None if owner_is_us => ServerIdentity::Ours,
        None => ServerIdentity::Unverifiable("server process token".into()),
    }
}

/// String SID of the owner of an object (`GetSecurityInfo(OWNER)`; a client pipe handle
/// opened for read has `READ_CONTROL`).
fn object_owner_sid(h: HANDLE, kind: SE_OBJECT_TYPE) -> io::Result<String> {
    // SAFETY: `owner` points into `sd`, which is LocalAlloc'd and freed after the conversion;
    // the string is freed with LocalFree.
    unsafe {
        let mut owner: PSID = std::ptr::null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        let rc = GetSecurityInfo(
            h,
            kind,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut sd,
        );
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc as i32));
        }
        if owner.is_null() {
            LocalFree(sd.cast());
            return Err(io::Error::other("object has no owner"));
        }
        let mut s: *mut u16 = std::ptr::null_mut();
        let ok = ConvertSidToStringSidW(owner, &mut s);
        let out = if ok == 0 {
            Err(io::Error::last_os_error())
        } else {
            let v = from_wide_ptr(s);
            LocalFree(s.cast());
            Ok(v)
        };
        LocalFree(sd.cast());
        out
    }
}

/// String SID of the token user of process `pid`, `None` when the process or its token can't
/// be opened (gone, or another account's process).
fn process_user_sid(pid: u32) -> Option<String> {
    // SAFETY: handles checked and closed once; the 512-byte 8-aligned buffer holds TOKEN_USER;
    // the SID string is freed with LocalFree.
    unsafe {
        let p = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if p.is_null() {
            return None;
        }
        let mut tok: HANDLE = std::ptr::null_mut();
        let ok = OpenProcessToken(p, TOKEN_QUERY, &mut tok);
        CloseHandle(p);
        if ok == 0 {
            return None;
        }
        let mut buf = vec![0u64; 64];
        let mut len = 0u32;
        let ok = GetTokenInformation(tok, TokenUser, buf.as_mut_ptr().cast(), 512, &mut len);
        CloseHandle(tok);
        if ok == 0 {
            return None;
        }
        let tu = &*(buf.as_ptr().cast::<TOKEN_USER>());
        let mut s: *mut u16 = std::ptr::null_mut();
        if ConvertSidToStringSidW(tu.User.Sid, &mut s) == 0 {
            return None;
        }
        let out = from_wide_ptr(s);
        LocalFree(s.cast());
        Some(out)
    }
}

/// M5-E S-01 (R38): who serves this client pipe handle? Checks, before any byte is sent,
/// that the pipe's owner SID **and** the token user of the server process
/// (`GetNamedPipeServerProcessId`, reported by the kernel) are `expected_sid`. Another
/// account that squats the predictable pipe name fails both (it can't make Alice the owner,
/// and its process runs as itself). Errors to read the owner count as not ours (fail closed).
pub fn pipe_server_identity(pipe: &impl AsRawHandle, expected_sid: &str) -> ServerIdentity {
    let h = raw(pipe);
    let owner = match object_owner_sid(h, SE_KERNEL_OBJECT) {
        Ok(o) => o,
        Err(e) => return ServerIdentity::Unverifiable(format!("pipe owner: {e}")),
    };
    let mut pid = 0u32;
    // SAFETY: valid client pipe handle; `pid` outlives the call.
    if unsafe { GetNamedPipeServerProcessId(h, &mut pid) } == 0 {
        return ServerIdentity::Unverifiable(format!(
            "server process id: {}",
            io::Error::last_os_error()
        ));
    }
    decide_server_identity(expected_sid, &owner, process_user_sid(pid).as_deref())
}

/// M5-E S-02 (R39): owner SID of a file or folder. Opened with `READ_CONTROL` only (no data
/// access) and `FILE_FLAG_OPEN_REPARSE_POINT`, so a junction is judged itself, never its target.
pub fn path_owner_sid(path: &Path) -> io::Result<String> {
    use std::os::windows::fs::OpenOptionsExt;
    let f = OpenOptions::new()
        .access_mode(READ_CONTROL)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    object_owner_sid(raw(&f), SE_FILE_OBJECT)
}

/// [`pipe_server_identity`] against the current user.
pub fn pipe_server_is_ours(pipe: &impl AsRawHandle) -> io::Result<ServerIdentity> {
    Ok(pipe_server_identity(pipe, &user_sid_string()?))
}

/// Effective owner + DACL + label of a kernel object, as SDDL (`GetSecurityInfo`).
pub fn handle_sddl(h: RawHandle) -> io::Result<String> {
    let info = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION | LABEL_SECURITY_INFORMATION;
    security_sddl(h.cast(), SE_KERNEL_OBJECT, info)
}

/// `GetSecurityInfo(h, kind, info)` as SDDL.
fn security_sddl(h: HANDLE, kind: SE_OBJECT_TYPE, info: u32) -> io::Result<String> {
    // SAFETY: descriptor and string are LocalAlloc'd and freed.
    unsafe {
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        let rc = GetSecurityInfo(
            h,
            kind,
            info,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut sd,
        );
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc as i32));
        }
        let mut s: *mut u16 = std::ptr::null_mut();
        let ok = ConvertSecurityDescriptorToStringSecurityDescriptorW(
            sd,
            SDDL_REVISION_1,
            info,
            &mut s,
            std::ptr::null_mut(),
        );
        LocalFree(sd.cast());
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let out = from_wide_ptr(s);
        LocalFree(s.cast());
        Ok(out)
    }
}

/// M5-E S-02 (R39): owner + DACL of a file or folder as SDDL (`READ_CONTROL` handle, reparse
/// point itself, never its target).
pub fn path_security_sddl(path: &Path) -> io::Result<String> {
    use std::os::windows::fs::OpenOptionsExt;
    let f = OpenOptions::new()
        .access_mode(READ_CONTROL)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    security_sddl(
        raw(&f),
        SE_FILE_OBJECT,
        OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
    )
}

/// M5-E S-02 (R39): in **one** `SetNamedSecurityInfoW` call, give folder `path` a protected DACL
/// with exactly two inheritable full-control ACEs (`sid` and SYSTEM) and, with `take_owner`,
/// owner `sid`. One atomic write: no moment with inherited or foreign ACEs (an `icacls /reset`
/// followed by a grant would leave one, and a handle opened then keeps its access). Children
/// that inherit get the new ACEs through the API's automatic propagation.
pub fn set_private_dacl(path: &Path, sid: &str, take_owner: bool) -> io::Result<()> {
    if !sid.starts_with("S-1-") || !sid.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "bad SID"));
    }
    let sddl = format!("O:{sid}D:P(A;OICI;FA;;;{sid})(A;OICI;FA;;;SY)");
    let w = wide(&sddl);
    let name = wide(path);
    // SAFETY: `sd` is LocalAlloc'd by the conversion and freed once; `owner` and `dacl` point
    // into it and are used only before the free; `name` is NUL-terminated and outlives the call.
    unsafe {
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            w.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            std::ptr::null_mut(),
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let mut present = 0;
        let mut defaulted = 0;
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut owner: PSID = std::ptr::null_mut();
        let ok = GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted) != 0
            && present != 0
            && !dacl.is_null()
            && GetSecurityDescriptorOwner(sd, &mut owner, &mut defaulted) != 0
            && !owner.is_null();
        if !ok {
            let e = io::Error::last_os_error();
            LocalFree(sd.cast());
            return Err(e);
        }
        let mut info = DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION;
        if take_owner {
            info |= OWNER_SECURITY_INFORMATION;
        }
        let rc = SetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            info,
            if take_owner {
                owner
            } else {
                std::ptr::null_mut()
            },
            std::ptr::null_mut(),
            dacl,
            std::ptr::null(),
        );
        LocalFree(sd.cast());
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc as i32));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dll_search_is_hardened() {
        assert!(harden_dll_search());
        assert!(harden_dll_search(), "idempotent");
    }

    #[tokio::test]
    async fn exclusive_bind_blocks_a_reuseaddr_squatter() {
        let ours = tokio::net::TcpSocket::new_v4().unwrap();
        set_exclusive_addr_use(&ours).unwrap();
        ours.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = ours.local_addr().unwrap();
        let _l = ours.listen(8).unwrap();
        // A same-user process asking for SO_REUSEADDR can't bind next to us.
        let squatter = tokio::net::TcpSocket::new_v4().unwrap();
        squatter.set_reuseaddr(true).unwrap();
        assert!(squatter.bind(addr).is_err());
    }

    #[test]
    fn canonical_sid_expands_aliases() {
        assert_eq!(canonical_sid("SY").unwrap(), "S-1-5-18");
        assert_eq!(canonical_sid("BA").unwrap(), ADMINISTRATORS_SID);
        let me = user_sid_string().unwrap();
        assert_eq!(canonical_sid(&me).unwrap(), me);
        assert!(canonical_sid("not a sid").is_err());
    }

    #[test]
    fn quoting_follows_argv_rules() {
        assert_eq!(quote_arg("plain"), "plain");
        assert_eq!(quote_arg(""), "\"\"");
        assert_eq!(quote_arg(r"C:\a b\"), r#""C:\a b\\""#);
        assert_eq!(quote_arg(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(quote_arg(r"C:\x y\z"), r#""C:\x y\z""#);
    }

    #[test]
    fn sid_and_dirs_resolve() {
        let sid = user_sid_string().unwrap();
        assert!(sid.starts_with("S-1-"), "{sid}");
        assert!(local_appdata().unwrap().is_absolute());
        assert!(system_dir().unwrap().join("schtasks.exe").exists());
        let sec = PipeSecurity::for_sid(&sid).unwrap();
        assert!(sec.sddl().ends_with("S:(ML;;NWNR;;;ME)"));
        assert!(
            PipeSecurity::for_sid("S-1-5)(A;;GA;;;WD").is_err(),
            "no SDDL injection"
        );
    }

    #[test]
    fn server_identity_decision_table() {
        let me = "S-1-5-21-1-2-3-1001";
        let other = "S-1-5-21-1-2-3-1002";
        use ServerIdentity as S;
        assert_eq!(decide_server_identity(me, me, Some(me)), S::Ours);
        // SID strings compare case-insensitively ("s-1-5-..." never appears, but be lenient).
        assert_eq!(
            decide_server_identity(me, &me.to_lowercase(), Some(me)),
            S::Ours
        );
        // Elevated server of the same user: owner Administrators, process user us.
        assert_eq!(
            decide_server_identity(me, ADMINISTRATORS_SID, Some(me)),
            S::Ours
        );
        // Squatter: its own owner, whatever the process query says.
        assert_eq!(
            decide_server_identity(me, other, None),
            S::ForeignOwner(other.into())
        );
        assert_eq!(
            decide_server_identity(me, other, Some(other)),
            S::ForeignOwner(other.into())
        );
        // Owner looks fine but the process runs as someone else.
        assert_eq!(
            decide_server_identity(me, me, Some(other)),
            S::ForeignProcess(other.into())
        );
        assert_eq!(
            decide_server_identity(me, ADMINISTRATORS_SID, Some(other)),
            S::ForeignProcess(other.into())
        );
        // Token unreadable: the exact owner suffices, Administrators alone doesn't.
        assert_eq!(decide_server_identity(me, me, None), S::Ours);
        assert!(matches!(
            decide_server_identity(me, ADMINISTRATORS_SID, None),
            S::Unverifiable(_)
        ));
        // Well-known SIDs are never "us".
        assert!(matches!(
            decide_server_identity(me, "S-1-5-18", Some("S-1-5-18")),
            S::ForeignOwner(_)
        ));
    }

    /// Live: a pipe this process serves with the daemon's descriptor is ours.
    #[tokio::test]
    async fn own_pipe_server_is_ours() {
        let name = format!(r"\\.\pipe\sr-m5e-s01-own-{}", std::process::id());
        let sec = PipeSecurity::current_user().unwrap();
        let _server = sec.create_server(&name, true).unwrap();
        let client = tokio::net::windows::named_pipe::ClientOptions::new()
            .open(&name)
            .unwrap();
        assert_eq!(pipe_server_is_ours(&client).unwrap(), ServerIdentity::Ours);
        // A different expected SID is refused (simulates the cross-account squat from the
        // victim's side: the pipe's owner is not the victim).
        // (Elevated test runs own the pipe as Administrators → ForeignProcess instead.)
        let other = "S-1-5-21-1-2-3-4242";
        assert!(matches!(
            pipe_server_identity(&client, other),
            ServerIdentity::ForeignOwner(_) | ServerIdentity::ForeignProcess(_)
        ));
    }

    #[test]
    fn lock_file_is_exclusive() {
        let dir = std::env::temp_dir().join(format!("sr-m1c-lock-{}", std::process::id()));
        let p = dir.join("x.lock");
        let a = lock_file(&p, false).unwrap();
        assert!(a.is_some());
        // A second handle in the same process conflicts too (LockFileEx is per handle).
        assert!(lock_file(&p, false).unwrap().is_none());
        drop(a);
        assert!(lock_file(&p, false).unwrap().is_some());
        let _ = std::fs::remove_dir_all(dir);
    }
}
