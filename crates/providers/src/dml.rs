//! DirectML DLL loading (Amendment 2 "Security", Amendment 3.3, §2.6 "DLL hijack").
//!
//! The pyke static ONNX Runtime imports `DirectML.dll`, `d3d12.dll` and `dxgi.dll`. Every binary
//! that links it **delay-loads** them ([`DELAYLOAD_ARGS`]), so nothing is mapped by name at start.
//! Before the first DirectML session, [`load_directml`]:
//!
//! 1. hardens the process DLL search ([`harden_dll_search`]: `SetDefaultDllDirectories(SYSTEM32 |
//!    APPLICATION_DIR)`) and maps `d3d12.dll` / `dxgi.dll` from System32 only;
//! 2. for each candidate (`<app dir>\DirectML.dll`, then `System32\DirectML.dll`; absolute, local,
//!    non-UNC paths only): opens the file **denying writers**, checks its embedded Authenticode
//!    signature with `WinVerifyTrust` (offline: no revocation fetch, cache-only URL retrieval, so
//!    no network traffic), requires leaf `O=Microsoft Corporation` chaining to a `Microsoft Root
//!    Certificate Authority`, and only then `LoadLibraryExW(<absolute path>)` while the handle is
//!    still held (no swap between check and load);
//! 3. confirms the mapped module's path is the verified one.
//!
//! The delay-load helper resolves `DirectML.dll` by name at the first DML call; because a module
//! with that base name is already loaded, the loader binds to it. If a *different* `DirectML.dll`
//! is already mapped (not delay-loaded, or planted), DirectML is refused ([`DmlError::Untrusted`])
//! and the caller silently uses the CPU.
//!
//! The result is computed once per process ([`OnceLock`]) and never unloaded.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Linker arguments every executable that links `sr-providers/local` must add (sr-daemon's
/// build.rs: `println!("cargo:rustc-link-arg-bins={a}")` for each).
pub const DELAYLOAD_ARGS: [&str; 4] = [
    "/DELAYLOAD:DirectML.dll",
    "/DELAYLOAD:d3d12.dll",
    "/DELAYLOAD:dxgi.dll",
    "delayimp.lib",
];

/// A verified, loaded `DirectML.dll`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DmlModule {
    pub path: PathBuf,
    /// Leaf certificate subject organisation (always "Microsoft Corporation").
    pub signer: String,
    /// Root CA common name.
    pub root: String,
    /// `VS_FIXEDFILEINFO` file version (`a.b.c.d`), always ≥ [`MIN_DIRECTML_VERSION`].
    pub version: FileVersion,
}

/// A PE file version, `[major, minor, build, revision]`.
pub type FileVersion = [u16; 4];

/// Oldest `DirectML.dll` accepted (O2, Amendment 2: "DirectML.dll 1.15.x"). A Microsoft-signed but
/// older DirectML (a downgrade attack, or a stale System32 copy) is refused like an untrusted one:
/// the provider silently stays on the CPU. Both copies on the dev box pass (app-local 1.15.4,
/// System32 1.15.6).
pub const MIN_DIRECTML_VERSION: FileVersion = [1, 15, 0, 0];

/// `v >= floor` (lexicographic over major, minor, build, revision).
pub fn version_at_least(v: FileVersion, floor: FileVersion) -> bool {
    v >= floor
}

fn fmt_version(v: FileVersion) -> String {
    format!("{}.{}.{}.{}", v[0], v[1], v[2], v[3])
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DmlError {
    /// Not Windows.
    Unsupported,
    /// No candidate file exists.
    Missing,
    /// Signature invalid / not Microsoft / unexpected module already mapped.
    Untrusted { path: PathBuf, reason: String },
    /// Signed by Microsoft, but older than [`MIN_DIRECTML_VERSION`] (or no version resource).
    TooOld {
        path: PathBuf,
        version: Option<FileVersion>,
        floor: FileVersion,
    },
    /// `LoadLibraryExW` failed (Win32 error code).
    LoadFailed { path: PathBuf, code: u32 },
}

impl std::fmt::Display for DmlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DmlError::Unsupported => write!(f, "DirectML is Windows-only"),
            DmlError::Missing => write!(f, "DirectML.dll not found in the app dir or System32"),
            DmlError::Untrusted { path, reason } => {
                write!(f, "DirectML.dll at {} refused: {reason}", path.display())
            }
            DmlError::TooOld {
                path,
                version,
                floor,
            } => write!(
                f,
                "DirectML.dll at {} refused: version {} is older than {}",
                path.display(),
                version.map_or_else(|| "unknown".to_owned(), fmt_version),
                fmt_version(*floor)
            ),
            DmlError::LoadFailed { path, code } => {
                write!(f, "LoadLibraryExW({}) failed: {code}", path.display())
            }
        }
    }
}

/// File version of a PE file (`GetFileVersionInfoW` from System32's `version.dll`, resolved at
/// run time: `version.dll` is not a KnownDLL and must never be imported by name).
pub fn file_version(path: &Path) -> Option<FileVersion> {
    imp::file_version(path)
}

/// Default candidates: the executable's directory, then System32.
pub fn default_search_dirs() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
    {
        v.push(dir);
    }
    if let Some(sys) = imp::system32() {
        v.push(sys);
    }
    v
}

static LOADED: OnceLock<Result<DmlModule, DmlError>> = OnceLock::new();

/// Load a verified `DirectML.dll` from the first acceptable candidate dir (process-wide, once;
/// later calls return the first result whatever `dirs` they pass).
pub fn load_directml(dirs: &[PathBuf]) -> Result<DmlModule, DmlError> {
    LOADED
        .get_or_init(|| imp::load(dirs, MIN_DIRECTML_VERSION))
        .clone()
}

/// Uncached [`load_directml`] (tests of the refusal paths in a dedicated process only).
#[doc(hidden)]
pub fn load_directml_uncached(dirs: &[PathBuf]) -> Result<DmlModule, DmlError> {
    imp::load(dirs, MIN_DIRECTML_VERSION)
}

/// Uncached load with a different version floor (tests drive the `TooOld` path with the real,
/// signed 1.15.x files by raising the floor). Dedicated test process only.
#[doc(hidden)]
pub fn load_directml_with_floor(
    dirs: &[PathBuf],
    floor: FileVersion,
) -> Result<DmlModule, DmlError> {
    imp::load(dirs, floor)
}

/// Whether a module with this base name is mapped, and from where.
pub fn loaded_module_path(name: &str) -> Option<PathBuf> {
    imp::module_path(name)
}

/// `SetDefaultDllDirectories(SYSTEM32 | APPLICATION_DIR)` and map `dxgi.dll` from System32 (ORT
/// enumerates adapters through it even on the CPU path). Idempotent; call before the first ORT
/// session. Returns false if hardening failed (logged; the absolute-path rules still apply).
pub fn harden_dll_search() -> bool {
    imp::harden()
}

/// Verify a file's embedded signature (exposed for diagnostics / tests).
pub fn verify_microsoft_signature(path: &Path) -> Result<(String, String), DmlError> {
    imp::verify_path(path)
}

#[cfg(not(windows))]
mod imp {
    use super::*;
    pub fn system32() -> Option<PathBuf> {
        None
    }
    pub fn load(_dirs: &[PathBuf], _floor: FileVersion) -> Result<DmlModule, DmlError> {
        Err(DmlError::Unsupported)
    }
    pub fn file_version(_p: &Path) -> Option<FileVersion> {
        None
    }
    pub fn module_path(_name: &str) -> Option<PathBuf> {
        None
    }
    pub fn harden() -> bool {
        true
    }
    pub fn verify_path(_p: &Path) -> Result<(String, String), DmlError> {
        Err(DmlError::Unsupported)
    }
}

#[cfg(windows)]
#[allow(unsafe_code)] // Win32 FFI: WinVerifyTrust, LoadLibraryExW, module queries. Reviewed (M1-D).
mod imp {
    use super::*;
    use std::ffi::{OsStr, OsString, c_void};
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::sync::atomic::{AtomicBool, Ordering};

    use windows_sys::Win32::Foundation::{
        CloseHandle, GENERIC_READ, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Security::Cryptography::{
        CERT_NAME_ATTR_TYPE, CertGetNameStringW, szOID_COMMON_NAME, szOID_ORGANIZATION_NAME,
    };
    use windows_sys::Win32::Security::WinTrust::{
        WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_FILE_INFO,
        WTD_CACHE_ONLY_URL_RETRIEVAL, WTD_CHOICE_FILE, WTD_REVOKE_NONE, WTD_STATEACTION_CLOSE,
        WTD_STATEACTION_VERIFY, WTD_UI_NONE, WTHelperGetProvSignerFromChain,
        WTHelperProvDataFromStateData, WinVerifyTrust,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::LibraryLoader::{
        GetModuleFileNameW, GetModuleHandleW, GetProcAddress, LOAD_LIBRARY_SEARCH_APPLICATION_DIR,
        LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
        SetDefaultDllDirectories,
    };
    use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;

    fn wide(s: &OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    pub fn system32() -> Option<PathBuf> {
        let mut buf = [0u16; 512];
        // SAFETY: buf is valid for buf.len() u16s; the API writes at most that many.
        let n = unsafe { GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } as usize;
        (n > 0 && n < buf.len()).then(|| PathBuf::from(OsString::from_wide(&buf[..n])))
    }

    static HARDENED: AtomicBool = AtomicBool::new(false);

    pub fn harden() -> bool {
        if HARDENED.load(Ordering::Acquire) {
            return true;
        }
        // SAFETY: plain flag call; affects later LoadLibrary calls of this process only.
        let ok = unsafe {
            SetDefaultDllDirectories(
                LOAD_LIBRARY_SEARCH_SYSTEM32 | LOAD_LIBRARY_SEARCH_APPLICATION_DIR,
            )
        } != 0;
        if !ok {
            tracing::warn!(err = last_error(), "SetDefaultDllDirectories failed");
        }
        // dxgi.dll is pulled by ORT on every path; make sure the System32 copy is the one mapped.
        let dxgi = load_system32("dxgi.dll");
        HARDENED.store(ok && dxgi, Ordering::Release);
        ok && dxgi
    }

    fn load_system32(name: &str) -> bool {
        let w = wide(OsStr::new(name));
        // SAFETY: w is NUL-terminated; LOAD_LIBRARY_SEARCH_SYSTEM32 restricts the search to
        // System32. The module is intentionally never freed.
        let h = unsafe {
            LoadLibraryExW(
                w.as_ptr(),
                std::ptr::null_mut(),
                LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
        };
        if h.is_null() {
            tracing::warn!(dll = name, err = last_error(), "System32 load failed");
            return false;
        }
        true
    }

    fn last_error() -> u32 {
        // SAFETY: trivially safe.
        unsafe { GetLastError() }
    }

    pub fn module_path(name: &str) -> Option<PathBuf> {
        let w = wide(OsStr::new(name));
        // SAFETY: w is NUL-terminated; no reference count is taken.
        let h = unsafe { GetModuleHandleW(w.as_ptr()) };
        if h.is_null() {
            return None;
        }
        let mut buf = vec![0u16; 1024];
        // SAFETY: h is a loaded module handle; buf valid for its length.
        let n = unsafe { GetModuleFileNameW(h, buf.as_mut_ptr(), buf.len() as u32) } as usize;
        (n > 0 && n < buf.len()).then(|| PathBuf::from(OsString::from_wide(&buf[..n])))
    }

    fn same_path(a: &Path, b: &Path) -> bool {
        a.as_os_str().to_string_lossy().to_lowercase()
            == b.as_os_str().to_string_lossy().to_lowercase()
    }

    /// Absolute, drive-letter, non-UNC, non-verbatim path.
    fn acceptable(p: &Path) -> bool {
        let s = p.as_os_str().to_string_lossy();
        p.is_absolute() && !s.starts_with(r"\\") && s.as_bytes().get(1) == Some(&b':')
    }

    struct Handle(HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: handle came from CreateFileW and is closed once.
            unsafe { CloseHandle(self.0) };
        }
    }

    /// Open denying writers and deleters for the lifetime of the returned handle.
    fn open_locked(p: &Path) -> Result<Handle, DmlError> {
        let w = wide(p.as_os_str());
        // SAFETY: w is NUL-terminated; no security attributes; no template.
        let h = unsafe {
            CreateFileW(
                w.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return Err(DmlError::Untrusted {
                path: p.to_path_buf(),
                reason: format!("open failed ({})", last_error()),
            });
        }
        Ok(Handle(h))
    }

    pub fn verify_path(p: &Path) -> Result<(String, String), DmlError> {
        let h = open_locked(p)?;
        verify(p, &h)
    }

    fn cert_name(
        cert: *const windows_sys::Win32::Security::Cryptography::CERT_CONTEXT,
        oid: windows_sys::core::PCSTR,
    ) -> String {
        let mut buf = [0u16; 256];
        // SAFETY: cert points into the WinVerifyTrust provider data, valid until the state is
        // closed (the caller holds it open); oid is a static NUL-terminated OID string.
        let n = unsafe {
            CertGetNameStringW(
                cert,
                CERT_NAME_ATTR_TYPE,
                0,
                oid as *const c_void,
                buf.as_mut_ptr(),
                buf.len() as u32,
            )
        } as usize;
        // n includes the terminating NUL; n == 1 means "not found".
        if n <= 1 {
            String::new()
        } else {
            String::from_utf16_lossy(&buf[..n - 1])
        }
    }

    fn verify(p: &Path, h: &Handle) -> Result<(String, String), DmlError> {
        let untrusted = |reason: String| DmlError::Untrusted {
            path: p.to_path_buf(),
            reason,
        };
        let wpath = wide(p.as_os_str());
        let mut file = WINTRUST_FILE_INFO {
            cbStruct: size_of::<WINTRUST_FILE_INFO>() as u32,
            pcwszFilePath: wpath.as_ptr(),
            hFile: h.0,
            pgKnownSubject: std::ptr::null_mut(),
        };
        let mut data = WINTRUST_DATA {
            cbStruct: size_of::<WINTRUST_DATA>() as u32,
            dwUIChoice: WTD_UI_NONE,
            fdwRevocationChecks: WTD_REVOKE_NONE,
            dwUnionChoice: WTD_CHOICE_FILE,
            dwStateAction: WTD_STATEACTION_VERIFY,
            dwProvFlags: WTD_CACHE_ONLY_URL_RETRIEVAL,
            ..Default::default()
        };
        data.Anonymous.pFile = &mut file;
        let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
        // SAFETY: data/file/wpath outlive both calls; INVALID_HANDLE_VALUE hwnd = no UI.
        let status = unsafe {
            WinVerifyTrust(
                INVALID_HANDLE_VALUE as _,
                &mut action,
                (&raw mut data).cast(),
            )
        };
        let result = (|| {
            if status != 0 {
                return Err(untrusted(format!("WinVerifyTrust 0x{:08X}", status as u32)));
            }
            // SAFETY: state data is valid after a VERIFY call until CLOSE.
            let prov = unsafe { WTHelperProvDataFromStateData(data.hWVTStateData) };
            if prov.is_null() {
                return Err(untrusted("no provider data".into()));
            }
            // SAFETY: prov valid (above); signer 0, no countersigner.
            let sgnr = unsafe { WTHelperGetProvSignerFromChain(prov, 0, 0, 0) };
            if sgnr.is_null() {
                return Err(untrusted("no signer".into()));
            }
            // SAFETY: sgnr valid; pasCertChain has csCertChain entries.
            let chain = unsafe {
                let s = &*sgnr;
                if s.csCertChain == 0 || s.pasCertChain.is_null() {
                    return Err(untrusted("empty chain".into()));
                }
                std::slice::from_raw_parts(s.pasCertChain, s.csCertChain as usize)
            };
            let leaf_o = cert_name(chain[0].pCert, szOID_ORGANIZATION_NAME);
            let root_cn = cert_name(chain[chain.len() - 1].pCert, szOID_COMMON_NAME);
            if leaf_o != "Microsoft Corporation" {
                return Err(untrusted(format!("signer O={leaf_o:?}")));
            }
            if !root_cn.starts_with("Microsoft Root Certificate Authority") {
                return Err(untrusted(format!("root CN={root_cn:?}")));
            }
            Ok((leaf_o, root_cn))
        })();
        data.dwStateAction = WTD_STATEACTION_CLOSE;
        // SAFETY: closes the state opened above.
        unsafe {
            WinVerifyTrust(
                INVALID_HANDLE_VALUE as _,
                &mut action,
                (&raw mut data).cast(),
            )
        };
        result
    }

    /// Version resource of `p` (`VS_FIXEDFILEINFO`), via System32's `version.dll`.
    pub fn file_version(p: &Path) -> Option<FileVersion> {
        use windows_sys::Win32::Storage::FileSystem::VS_FIXEDFILEINFO;
        type GetSize = unsafe extern "system" fn(*const u16, *mut u32) -> u32;
        type GetInfo = unsafe extern "system" fn(*const u16, u32, u32, *mut c_void) -> i32;
        type Query =
            unsafe extern "system" fn(*const c_void, *const u16, *mut *mut c_void, *mut u32) -> i32;
        let size = crate::sys::system32_proc("version.dll", c"GetFileVersionInfoSizeW")?;
        let info = crate::sys::system32_proc("version.dll", c"GetFileVersionInfoW")?;
        let query = crate::sys::system32_proc("version.dll", c"VerQueryValueW")?;
        // SAFETY: the three exports have exactly these documented signatures.
        let (size, info, query) = unsafe {
            (
                std::mem::transmute::<crate::sys::Proc, GetSize>(size),
                std::mem::transmute::<crate::sys::Proc, GetInfo>(info),
                std::mem::transmute::<crate::sys::Proc, Query>(query),
            )
        };
        let w = wide(p.as_os_str());
        let mut ignored = 0u32;
        // SAFETY: `w` is NUL-terminated; out pointer valid.
        let n = unsafe { size(w.as_ptr(), &mut ignored) };
        if n == 0 || n > 16 << 20 {
            return None;
        }
        let mut buf = vec![0u8; n as usize];
        // SAFETY: `buf` is valid for `n` bytes.
        if unsafe { info(w.as_ptr(), 0, n, buf.as_mut_ptr().cast()) } == 0 {
            return None;
        }
        let root = wide(OsStr::new("\\"));
        let mut out: *mut c_void = std::ptr::null_mut();
        let mut len = 0u32;
        // SAFETY: `buf` holds the version block GetFileVersionInfoW wrote; the returned pointer
        // points into `buf`, which outlives its use below.
        if unsafe { query(buf.as_ptr().cast(), root.as_ptr(), &mut out, &mut len) } == 0
            || out.is_null()
            || (len as usize) < size_of::<VS_FIXEDFILEINFO>()
        {
            return None;
        }
        // SAFETY: VerQueryValueW("\\") returns a VS_FIXEDFILEINFO inside `buf` (length checked);
        // read_unaligned because the block has no alignment guarantee.
        let fi = unsafe { std::ptr::read_unaligned(out.cast::<VS_FIXEDFILEINFO>()) };
        if fi.dwSignature != 0xFEEF_04BD {
            return None;
        }
        Some([
            (fi.dwFileVersionMS >> 16) as u16,
            (fi.dwFileVersionMS & 0xffff) as u16,
            (fi.dwFileVersionLS >> 16) as u16,
            (fi.dwFileVersionLS & 0xffff) as u16,
        ])
    }

    fn check_version(p: &Path, floor: FileVersion) -> Result<FileVersion, DmlError> {
        match file_version(p) {
            Some(v) if version_at_least(v, floor) => Ok(v),
            version => Err(DmlError::TooOld {
                path: p.to_path_buf(),
                version,
                floor,
            }),
        }
    }

    pub fn load(dirs: &[PathBuf], floor: FileVersion) -> Result<DmlModule, DmlError> {
        harden();
        // d3d12.dll: System32 only, before anything can resolve it by name.
        if !load_system32("d3d12.dll") {
            return Err(DmlError::Missing);
        }
        let candidates: Vec<PathBuf> = dirs
            .iter()
            .map(|d| d.join("DirectML.dll"))
            .filter(|p| acceptable(p) && p.is_file())
            .collect();
        if let Some(already) = module_path("DirectML.dll") {
            // Mapped before us: only acceptable if it is a candidate that verifies.
            return match candidates.iter().find(|c| same_path(c, &already)) {
                Some(c) => {
                    let (signer, root) = verify_path(c)?;
                    let version = check_version(c, floor)?;
                    Ok(DmlModule {
                        path: c.clone(),
                        signer,
                        root,
                        version,
                    })
                }
                None => Err(DmlError::Untrusted {
                    path: already,
                    reason: "a different DirectML.dll was already loaded (delay-load missing?)"
                        .into(),
                }),
            };
        }
        let mut last = DmlError::Missing;
        for c in candidates {
            let h = match open_locked(&c) {
                Ok(h) => h,
                Err(e) => {
                    last = e;
                    continue;
                }
            };
            let (signer, root) = match verify(&c, &h) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, "DirectML candidate refused");
                    last = e;
                    continue;
                }
            };
            // Read while `h` still denies writers (the same bytes the signature covered).
            let version = match check_version(&c, floor) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, "DirectML candidate refused");
                    last = e;
                    continue;
                }
            };
            let w = wide(c.as_os_str());
            // SAFETY: absolute path, verified while `h` denies writers; dependencies resolve from
            // the DLL's own dir and System32 only. Never freed (ORT holds function pointers).
            let m = unsafe {
                LoadLibraryExW(
                    w.as_ptr(),
                    std::ptr::null_mut(),
                    LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
                )
            };
            drop(h);
            if m.is_null() {
                last = DmlError::LoadFailed {
                    path: c.clone(),
                    code: last_error(),
                };
                continue;
            }
            // A different Microsoft-signed DLL renamed to DirectML.dll passes the signature check;
            // it must also export what ORT's DML EP calls, or the delay-load helper would raise
            // a fatal exception at the first DirectML call.
            // SAFETY: m is a valid module handle; the name is a static NUL-terminated string.
            if unsafe { GetProcAddress(m, windows_sys::core::s!("DMLCreateDevice1")) }.is_none() {
                return Err(DmlError::Untrusted {
                    path: c,
                    reason: "signed, but not DirectML (no DMLCreateDevice1 export)".into(),
                });
            }
            return match module_path("DirectML.dll") {
                Some(mapped) if same_path(&mapped, &c) => Ok(DmlModule {
                    path: c,
                    signer,
                    root,
                    version,
                }),
                other => Err(DmlError::Untrusted {
                    path: other.unwrap_or_default(),
                    reason: "mapped module path differs from the verified file".into(),
                }),
            };
        }
        Err(last)
    }
}
