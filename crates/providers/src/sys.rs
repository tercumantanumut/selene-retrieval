//! System32-only symbol resolution (§2.6 "DLL hijack", Amendment 4.8).
//!
//! `dxgi.dll`, `d3d12.dll`, `version.dll` and `shell32.dll` are resolved at run time from
//! **System32 by absolute search** (`LOAD_LIBRARY_SEARCH_SYSTEM32`) and called through
//! `GetProcAddress`, never imported by name: `version.dll` is not a KnownDLL, so a load-time import
//! would let a planted copy in the (per-user, writable) install dir load before `main`, and the
//! GPU DLLs must stay delay-loaded (sr-daemon's `/DELAYLOAD`). Modules are never freed.

/// A resolved export (`FARPROC`); callers transmute it to the documented signature.
#[cfg(windows)]
pub(crate) type Proc = unsafe extern "system" fn() -> isize;

/// `GetProcAddress(LoadLibraryExW(<System32>\dll), name)`; `None` if either step fails.
#[cfg(windows)]
#[allow(unsafe_code)] // Win32 FFI: LoadLibraryExW + GetProcAddress. Reviewed (M2-G).
pub(crate) fn system32_proc(dll: &str, name: &std::ffi::CStr) -> Option<Proc> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::LibraryLoader::{
        GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
    };
    let w: Vec<u16> = OsStr::new(dll)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `w` is NUL-terminated; LOAD_LIBRARY_SEARCH_SYSTEM32 restricts the search to
    // System32 (no app dir, no PATH, no cwd). The module is intentionally never freed, so the
    // returned function pointer stays valid for the life of the process.
    let h = unsafe {
        LoadLibraryExW(
            w.as_ptr(),
            std::ptr::null_mut(),
            LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
    };
    if h.is_null() {
        return None;
    }
    // SAFETY: `h` is a valid module handle; `name` is a NUL-terminated C string.
    unsafe { GetProcAddress(h, name.as_ptr().cast()) }
}

/// Free bytes available to this user on the volume holding `path` (`GetDiskFreeSpaceExW`;
/// kernel32 is a KnownDLL). `None` if the query fails.
#[cfg(windows)]
#[allow(unsafe_code)] // Win32 FFI: GetDiskFreeSpaceExW. Reviewed (M2-G).
pub fn free_disk(path: &std::path::Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    // The directory may not exist yet: ask about the nearest existing ancestor.
    let mut p = path;
    while !p.exists() {
        p = p.parent()?;
    }
    let w: Vec<u16> = p
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut avail = 0u64;
    // SAFETY: `w` is NUL-terminated; the out pointer is valid; the other outputs are optional.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            w.as_ptr(),
            &mut avail,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(avail)
}

/// Free bytes available to this user on the volume holding `path` (`statvfs`: `f_bavail`, the
/// blocks a non-root user may use). `None` if the query fails.
#[cfg(unix)]
#[allow(unsafe_code)] // libc FFI: statvfs. Reviewed (macOS port).
pub fn free_disk(path: &std::path::Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    // The directory may not exist yet: ask about the nearest existing ancestor.
    let mut p = path;
    while !p.exists() {
        p = p.parent()?;
    }
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).ok()?;
    // SAFETY: an all-zero statvfs is a valid out-struct.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is NUL-terminated and `st` is a valid, writable statvfs.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    // Field widths differ between platforms (u32 / u64), hence the casts.
    #[allow(clippy::unnecessary_cast)]
    let (avail, frsize) = (st.f_bavail as u64, st.f_frsize as u64);
    Some(avail.saturating_mul(frsize))
}

#[cfg(not(any(windows, unix)))]
pub fn free_disk(_path: &std::path::Path) -> Option<u64> {
    None
}

// CLI discovery (`cli::discover`, feature `remote`) on Unix.

/// Real user id of this process.
#[cfg(all(unix, feature = "remote"))]
#[allow(unsafe_code)] // libc FFI: getuid. Reviewed (macOS port).
pub(crate) fn uid() -> u32 {
    // SAFETY: no arguments; getuid cannot fail.
    unsafe { libc::getuid() }
}

/// Home folder of the current user from the user database (`getpwuid_r`), never `$HOME` (an MCP
/// client or a launcher may pass another environment). `None` if there is none or it is relative.
#[cfg(all(unix, feature = "remote"))]
#[allow(unsafe_code)] // libc FFI: getpwuid_r. Reviewed (macOS port).
pub(crate) fn home_dir() -> Option<std::path::PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let mut buf = vec![0u8; 4096];
    loop {
        // SAFETY: an all-zero `passwd` is a valid out value (null pointers, zero ids).
        let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
        let mut res: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: `pw`, `buf` and `res` outlive the call and `buf.len()` is the buffer size; the
        // strings in `pw` point into `buf` and are read before `buf` changes.
        let rc = unsafe {
            libc::getpwuid_r(
                libc::getuid(),
                &mut pw,
                buf.as_mut_ptr().cast(),
                buf.len(),
                &mut res,
            )
        };
        if rc == libc::ERANGE && buf.len() < 1 << 20 {
            buf.resize(buf.len() * 4, 0);
            continue;
        }
        if rc != 0 || res.is_null() || pw.pw_dir.is_null() {
            return None;
        }
        // SAFETY: on success `pw_dir` is a NUL-terminated string inside `buf`.
        let dir = unsafe { std::ffi::CStr::from_ptr(pw.pw_dir) }
            .to_bytes()
            .to_vec();
        let dir = std::path::PathBuf::from(std::ffi::OsString::from_vec(dir));
        return dir.is_absolute().then_some(dir);
    }
}

/// Whether `path` lies on a local volume (not a network share). macOS: `statfs` `MNT_LOCAL`.
#[cfg(all(target_os = "macos", feature = "remote"))]
#[allow(unsafe_code)] // libc FFI: statfs. Reviewed (macOS port).
pub(crate) fn on_local_volume(path: &std::path::Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: an all-zero statfs is a valid out-struct.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is NUL-terminated and `st` is a valid, writable statfs.
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return false;
    }
    st.f_flags & (libc::MNT_LOCAL as u32) != 0
}

/// Other Unix systems: no portable flag; treated as local.
#[cfg(all(unix, not(target_os = "macos"), feature = "remote"))]
pub(crate) fn on_local_volume(_path: &std::path::Path) -> bool {
    true
}
