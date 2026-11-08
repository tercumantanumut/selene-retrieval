//! System32-only symbol resolution (§2.6 "DLL hijack", Amendment 4.8).
//!
//! `dxgi.dll`, `d3d12.dll`, `version.dll` and `shell32.dll` are resolved at run time from
//! **System32 by absolute search** (`LOAD_LIBRARY_SEARCH_SYSTEM32`) and called through
//! `GetProcAddress`, never imported by name: `version.dll` is not a KnownDLL, so a load-time import
//! would let a planted copy in the (per-user, writable) install dir load before `main`, and the
//! GPU DLLs must stay delay-loaded (sr-daemon's `/DELAYLOAD`). Modules are never freed.

/// A resolved export (`FARPROC`); callers transmute it to the documented signature.
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

#[cfg(not(windows))]
pub(crate) fn system32_proc(_dll: &str, _name: &std::ffi::CStr) -> Option<Proc> {
    None
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

#[cfg(not(windows))]
pub fn free_disk(_path: &std::path::Path) -> Option<u64> {
    None
}
