//! Shared between sr-bridge and sr-daemon (included via `#[path]` so the bridge
//! keeps a dependency set of exactly tokio + windows-sys).
#![allow(dead_code)]

use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::AsRawHandle;
use std::path::PathBuf;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
use windows_sys::Win32::Storage::FileSystem::{
    LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
};
use windows_sys::Win32::System::IO::OVERLAPPED;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::UI::Shell::{FOLDERID_LocalAppData, KF_FLAG_DONT_VERIFY, SHGetKnownFolderPath};
use windows_sys::Win32::System::Com::CoTaskMemFree;

/// Wire handshake version (sr daemon protocol, not MCP). The daemon accepts
/// `MIN_PROTO..=PROTO` (N-1..N); anything older is refused with a plain message.
pub const PROTO: u16 = 3;
pub const MIN_PROTO: u16 = 2;
/// Build string carried in the handshake (semver of the shipped bundle).
pub const BUILD: &str = "0.0.1-m0d";
/// Plain-language refusal text (U8-style: one sentence + one next action).
pub const MSG_CLIENT_OUTDATED: &str =
    "Selene Retrieval was updated and this connector is out of date. Restart your AI app to reconnect.";
pub const MSG_DAEMON_OUTDATED: &str =
    "Selene Retrieval is finishing an update. Wait a few seconds, then restart your AI app.";
/// Scheduled task used by the spike. Production name would be \SeleneRetrieval\Service.
pub const TASK_NAME: &str = "\\SeleneRetrievalSpike\\Daemon";

pub fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

pub unsafe fn from_wide_ptr(p: *const u16) -> String {
    let mut n = 0;
    while unsafe { *p.add(n) } != 0 {
        n += 1;
    }
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, n) })
}

/// String SID of the current process token user, e.g. "S-1-5-21-...-1001".
pub fn user_sid_string() -> io::Result<String> {
    unsafe {
        let mut tok: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok) == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buf = vec![0u8; 256];
        let mut len = 0u32;
        let ok = GetTokenInformation(tok, TokenUser, buf.as_mut_ptr().cast(), buf.len() as u32, &mut len);
        CloseHandle(tok);
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let tu = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut s: *mut u16 = std::ptr::null_mut();
        if ConvertSidToStringSidW(tu.User.Sid, &mut s) == 0 {
            return Err(io::Error::last_os_error());
        }
        let out = from_wide_ptr(s);
        LocalFree(s.cast());
        Ok(out)
    }
}

/// FNV-1a 64. Production uses blake3 (sr-common); the spike bridge keeps zero extra deps.
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

pub fn local_appdata() -> PathBuf {
    unsafe {
        let mut p: *mut u16 = std::ptr::null_mut();
        let hr = SHGetKnownFolderPath(&FOLDERID_LocalAppData, KF_FLAG_DONT_VERIFY as u32, std::ptr::null_mut(), &mut p);
        if hr == 0 && !p.is_null() {
            let s = from_wide_ptr(p);
            CoTaskMemFree(p.cast());
            return PathBuf::from(s);
        }
    }
    PathBuf::from(std::env::var_os("LOCALAPPDATA").expect("LOCALAPPDATA"))
}

/// %LOCALAPPDATA%\SR-spike\data unless SR_DATA_DIR overrides.
/// (Spike-only directory so the test never collides with a real install. Compiled in, not
/// env-only, because a task-launched daemon does not inherit the bridge's environment.)
pub fn data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("SR_DATA_DIR") {
        return PathBuf::from(d);
    }
    local_appdata().join("SR-spike").join("data")
}

/// \\.\pipe\sr-<sidhash16>-<datadirhash16>
pub fn pipe_name() -> String {
    let sid = user_sid_string().unwrap_or_default();
    let dd = data_dir().to_string_lossy().to_lowercase();
    format!(
        r"\\.\pipe\sr-{:016x}-{:016x}",
        fnv1a64(sid.as_bytes()),
        fnv1a64(dd.as_bytes())
    )
}

/// Opens (creating) `name` in the data dir and takes an exclusive LockFileEx lock.
/// `wait=false` uses LOCKFILE_FAIL_IMMEDIATELY. The lock lives as long as the File.
pub fn lock_file(name: &str, wait: bool) -> io::Result<File> {
    let dir = data_dir();
    std::fs::create_dir_all(&dir)?;
    let f = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(dir.join(name))?;
    let mut flags = LOCKFILE_EXCLUSIVE_LOCK;
    if !wait {
        flags |= LOCKFILE_FAIL_IMMEDIATELY;
    }
    unsafe {
        let mut ov: OVERLAPPED = std::mem::zeroed();
        if LockFileEx(f.as_raw_handle() as HANDLE, flags, 0, 1, 0, &mut ov) == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(f)
}

pub fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Appends a line to <data_dir>\<file>; best effort.
pub fn log_line(file: &str, line: &str) {
    use std::io::Write;
    let dir = data_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(dir.join(file)) {
        // One write_all per line so concurrent appenders don't interleave.
        let _ = f.write_all(format!("{} pid={} {}
", now_ms(), std::process::id(), line).as_bytes());
    }
}
