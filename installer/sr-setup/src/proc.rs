//! Processes running from the install directory (uninstall/update must not leave an exe in use).
//!
//! The only `unsafe` in sr-setup: Toolhelp process snapshot, `QueryFullProcessImageNameW`,
//! `TerminateProcess`, `WaitForSingleObject`. Every handle is owned by [`Handle`] and closed on drop.
//! Only processes of the current user can be opened (no privilege is requested), so another user's
//! processes are never touched.
#![allow(unsafe_code)]

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    PROCESS_TERMINATE, QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
};

struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            // SAFETY: we own the handle and close it exactly once.
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// A process whose image lives in the install directory.
#[derive(Debug, Clone)]
pub struct Proc {
    pub pid: u32,
    pub image: PathBuf,
}

impl Proc {
    /// Lower-case file name (`sr-daemon.exe`).
    pub fn file_name(&self) -> String {
        self.image
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    }
}

fn open(pid: u32, access: u32) -> Option<Handle> {
    // SAFETY: plain call; a null return means no access (another user, protected, gone).
    let h = unsafe { OpenProcess(access, 0, pid) };
    (!h.is_null()).then_some(Handle(h))
}

fn image_of(h: &Handle) -> Option<PathBuf> {
    let mut buf = vec![0u16; 32_768];
    let mut len = buf.len() as u32;
    // SAFETY: `buf` holds `len` u16s; on success `len` is the number written (no NUL).
    let ok =
        unsafe { QueryFullProcessImageNameW(h.0, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) };
    (ok != 0).then(|| PathBuf::from(String::from_utf16_lossy(&buf[..len as usize])))
}

/// Comparable key of a path: canonical when it exists, lower-case, backslashes, no `\\?\`.
pub fn path_key(p: &Path) -> String {
    let c = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let s = c.to_string_lossy().replace('/', "\\");
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s).to_string();
    s.trim_end_matches('\\').to_lowercase()
}

/// Whether `image` is inside `dir` (any depth).
pub fn is_inside(image: &Path, dir: &Path) -> bool {
    let d = path_key(dir);
    !d.is_empty() && path_key(image).starts_with(&format!("{d}\\"))
}

/// Every process of this user whose image is inside `dir`, except the current process.
pub fn running_under(dir: &Path) -> io::Result<Vec<Proc>> {
    let me = std::process::id();
    // SAFETY: plain call; the handle is owned below.
    let snap = Handle(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) });
    if snap.0 == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: zeroed POD struct with dwSize set, as the API requires.
    let mut e: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut out = Vec::new();
    // SAFETY: valid snapshot handle and entry pointer.
    let mut more = unsafe { Process32FirstW(snap.0, &mut e) } != 0;
    while more {
        let pid = e.th32ProcessID;
        if pid != me
            && pid != 0
            && let Some(h) = open(pid, PROCESS_QUERY_LIMITED_INFORMATION)
            && let Some(image) = image_of(&h)
            && is_inside(&image, dir)
        {
            out.push(Proc { pid, image });
        }
        // SAFETY: as above.
        more = unsafe { Process32NextW(snap.0, &mut e) } != 0;
    }
    Ok(out)
}

/// Waits up to `timeout` for `pid` to exit. True when it is gone.
pub fn wait_exit(pid: u32, timeout: Duration) -> bool {
    let Some(h) = open(pid, PROCESS_SYNCHRONIZE) else {
        return true; // already gone (or not ours, in which case we never touch it)
    };
    let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
    // SAFETY: valid process handle with SYNCHRONIZE.
    unsafe { WaitForSingleObject(h.0, ms) == WAIT_OBJECT_0 }
}

/// Terminates `pid` (exit code 1) and waits up to `timeout` for it to go.
pub fn terminate(pid: u32, timeout: Duration) -> io::Result<bool> {
    let Some(h) = open(pid, PROCESS_TERMINATE | PROCESS_SYNCHRONIZE) else {
        return Ok(true);
    };
    // SAFETY: valid process handle with PROCESS_TERMINATE.
    if unsafe { TerminateProcess(h.0, 1) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
    // SAFETY: valid process handle with SYNCHRONIZE.
    Ok(unsafe { WaitForSingleObject(h.0, ms) == WAIT_OBJECT_0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inside_rule() {
        let d = Path::new(r"C:\Users\u\AppData\Local\Selene Retrieval");
        assert!(is_inside(
            Path::new(r"c:\users\U\appdata\local\selene retrieval\sr-daemon.exe"),
            d
        ));
        assert!(is_inside(
            Path::new(r"C:/Users/u/AppData/Local/Selene Retrieval/sub/x.exe"),
            d
        ));
        // Sibling with the same prefix is not inside.
        assert!(!is_inside(
            Path::new(r"C:\Users\u\AppData\Local\Selene Retrieval2\sr-daemon.exe"),
            d
        ));
        assert!(!is_inside(Path::new(r"C:\x.exe"), Path::new("")));
    }

    #[test]
    fn scan_finds_nothing_in_an_empty_dir() {
        let d = std::env::temp_dir().join(format!("sr-setup-scan-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap_or_else(|e| panic!("{e}"));
        let found = running_under(&d).unwrap_or_else(|e| panic!("{e}"));
        let _ = std::fs::remove_dir(&d);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn scan_sees_a_child_and_terminate_stops_it() {
        // Copy a long-running inbox exe into a temp "install dir", run it, find it, kill it.
        let d = std::env::temp_dir().join(format!("sr-setup-kill-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap_or_else(|e| panic!("{e}"));
        let sys = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_default();
        let exe = d.join("waiter.exe");
        std::fs::copy(sys.join("System32").join("PING.EXE"), &exe)
            .unwrap_or_else(|e| panic!("{e}"));
        let mut child = std::process::Command::new(&exe)
            .args(["-n", "60", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .stdin(std::process::Stdio::null())
            .spawn()
            .unwrap_or_else(|e| panic!("{e}"));
        let found = running_under(&d).unwrap_or_else(|e| panic!("{e}"));
        let ok = found
            .iter()
            .any(|p| p.pid == child.id() && p.file_name() == "waiter.exe");
        let gone = terminate(child.id(), Duration::from_secs(5)).unwrap_or(false);
        let _ = child.wait();
        let _ = std::fs::remove_file(&exe);
        let _ = std::fs::remove_dir(&d);
        assert!(ok, "child not found in {found:?}");
        assert!(gone);
    }
}
