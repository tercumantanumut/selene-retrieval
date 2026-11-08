//! Reviewed Win32 FFI for the engine (the only `unsafe` in sr-engine; add it to the reviewed list).
//!
//! Four read-only queries and one console attach, each a single documented call with a
//! caller-owned, correctly sized buffer:
//! * [`total_ram`] — `GlobalMemoryStatusEx`.
//! * [`physical_cores`] — `GetLogicalProcessorInformationEx(RelationProcessorCore)`.
//! * [`free_disk`] — `GetDiskFreeSpaceExW` (the data-dir volume; tier detection).
//! * [`file_identity`] — `GetFileInformationByHandleEx(FileIdInfo)` (128-bit NTFS id + volume
//!   serial; rename detection, docs/00 §0.A "file IDs"). std's equivalent is still unstable.
//! * [`attach_parent_console`] — `AttachConsole(ATTACH_PARENT_PROCESS)`, so the hidden `admin` CLI of
//!   a `windows_subsystem = "windows"` exe can print to the terminal that started it.
//! * M5-E (OneDrive): [`file_attributes`] — `GetFileAttributesW` on a NUL-terminated path we own
//!   (no handle, so a cloud placeholder is never downloaded by the check).
//! * M5-E (machine policy): [`program_data`] — `SHGetKnownFolderPath(FOLDERID_ProgramData)` (never
//!   the environment, which the user controls), buffer freed once with `CoTaskMemFree`; and
//!   [`policy_value`] — `RegGetValueW` on `HKLM` / `HKCU` `SOFTWARE\Policies\SeleneRetrieval`, read
//!   only, DWORD / SZ / MULTI_SZ, size query first, then a caller-owned `u16` buffer of that size.

use std::path::Path;

#[cfg(windows)]
#[allow(unsafe_code)]
mod imp {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;

    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ID_INFO, FileIdInfo, GetDiskFreeSpaceExW, GetFileInformationByHandleEx,
    };
    use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    use windows_sys::Win32::System::SystemInformation::{
        GetLogicalProcessorInformationEx, GlobalMemoryStatusEx, MEMORYSTATUSEX,
        RelationProcessorCore, SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
    };

    pub fn total_ram() -> Option<u64> {
        let mut m = MEMORYSTATUSEX {
            dwLength: u32::try_from(size_of::<MEMORYSTATUSEX>()).ok()?,
            dwMemoryLoad: 0,
            ullTotalPhys: 0,
            ullAvailPhys: 0,
            ullTotalPageFile: 0,
            ullAvailPageFile: 0,
            ullTotalVirtual: 0,
            ullAvailVirtual: 0,
            ullAvailExtendedVirtual: 0,
        };
        // SAFETY: `m` is a valid, writable MEMORYSTATUSEX with dwLength set as the API requires.
        let ok = unsafe { GlobalMemoryStatusEx(&mut m) };
        (ok != 0).then_some(m.ullTotalPhys)
    }

    pub fn physical_cores() -> Option<u32> {
        let mut len = 0u32;
        // SAFETY: size query: a null buffer with length 0 only writes the required length.
        unsafe {
            GetLogicalProcessorInformationEx(RelationProcessorCore, std::ptr::null_mut(), &mut len)
        };
        if len == 0 {
            return None;
        }
        // u64 backing store keeps the records 8-byte aligned.
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        // SAFETY: `buf` holds at least `len` writable bytes, suitably aligned for the records.
        let ok = unsafe {
            GetLogicalProcessorInformationEx(
                RelationProcessorCore,
                buf.as_mut_ptr()
                    .cast::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>(),
                &mut len,
            )
        };
        if ok == 0 {
            return None;
        }
        let bytes = buf.as_ptr().cast::<u8>();
        let (mut off, mut cores) = (0usize, 0u32);
        let header = size_of::<u32>() * 2; // Relationship + Size
        while off + header <= len as usize {
            // SAFETY: `off + header <= len`, inside the buffer the API filled; the first two
            // fields of every record are (LOGICAL_PROCESSOR_RELATIONSHIP: i32, Size: u32).
            let (rel, size) = unsafe {
                let p = bytes.add(off);
                (
                    std::ptr::read_unaligned(p.cast::<i32>()),
                    std::ptr::read_unaligned(p.add(4).cast::<u32>()),
                )
            };
            if size == 0 {
                break;
            }
            if rel == RelationProcessorCore {
                cores += 1;
            }
            off += size as usize;
        }
        (cores > 0).then_some(cores)
    }

    pub fn free_disk(path: &Path) -> Option<u64> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut free = 0u64;
        // SAFETY: `wide` is NUL-terminated; the out pointer is valid; the other outs may be null.
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut free,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        (ok != 0).then_some(free)
    }

    pub fn file_identity(f: &std::fs::File) -> Option<(u32, [u8; 16])> {
        let mut info = FILE_ID_INFO {
            VolumeSerialNumber: 0,
            FileId: windows_sys::Win32::Storage::FileSystem::FILE_ID_128 {
                Identifier: [0; 16],
            },
        };
        // SAFETY: `f` is an open file handle owned by the caller for the duration of the call;
        // `info` is a writable FILE_ID_INFO and the size passed matches it.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                f.as_raw_handle(),
                FileIdInfo,
                (&mut info as *mut FILE_ID_INFO).cast::<c_void>(),
                u32::try_from(size_of::<FILE_ID_INFO>()).ok()?,
            )
        };
        // The catalog keeps the low 32 bits of the serial, matching `volume_serial` elsewhere.
        (ok != 0).then_some((info.VolumeSerialNumber as u32, info.FileId.Identifier))
    }

    pub fn attach_parent_console() -> bool {
        // SAFETY: no pointers; fails harmlessly when there is no parent console.
        unsafe { AttachConsole(ATTACH_PARENT_PROCESS) != 0 }
    }

    pub fn file_attributes(path: &Path) -> Option<u32> {
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileAttributesW, INVALID_FILE_ATTRIBUTES,
        };
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: `wide` is NUL-terminated and outlives the call; no handle is opened for data,
        // so a cloud placeholder is not downloaded.
        let a = unsafe { GetFileAttributesW(wide.as_ptr()) };
        (a != INVALID_FILE_ATTRIBUTES).then_some(a)
    }

    pub fn program_data() -> Option<std::path::PathBuf> {
        use windows_sys::Win32::System::Com::CoTaskMemFree;
        use windows_sys::Win32::UI::Shell::{FOLDERID_ProgramData, SHGetKnownFolderPath};
        let mut p: *mut u16 = std::ptr::null_mut();
        // SAFETY: the GUID is a static; `p` receives a CoTaskMem string (or null) that is freed
        // exactly once below on every path.
        let hr =
            unsafe { SHGetKnownFolderPath(&FOLDERID_ProgramData, 0, std::ptr::null_mut(), &mut p) };
        if p.is_null() {
            return None;
        }
        let out = if hr == 0 {
            let mut n = 0usize;
            // SAFETY: on success `p` is a NUL-terminated UTF-16 string; we stop at the NUL.
            while unsafe { *p.add(n) } != 0 {
                n += 1;
            }
            // SAFETY: `n` units starting at `p` were just read as valid.
            let units = unsafe { std::slice::from_raw_parts(p, n) };
            Some(std::path::PathBuf::from(String::from_utf16_lossy(units)))
        } else {
            None
        };
        // SAFETY: `p` came from SHGetKnownFolderPath and is freed once.
        unsafe { CoTaskMemFree(p.cast()) };
        out
    }

    pub fn policy_value(machine: bool, subkey: &str, name: &str) -> Option<super::RegValue> {
        use windows_sys::Win32::Foundation::ERROR_SUCCESS;
        use windows_sys::Win32::System::Registry::{
            HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, REG_DWORD, REG_EXPAND_SZ, REG_MULTI_SZ, REG_SZ,
            RRF_NOEXPAND, RRF_RT_REG_DWORD, RRF_RT_REG_MULTI_SZ, RRF_RT_REG_SZ, RegGetValueW,
        };
        let hive = if machine {
            HKEY_LOCAL_MACHINE
        } else {
            HKEY_CURRENT_USER
        };
        let key: Vec<u16> = std::ffi::OsStr::new(subkey)
            .encode_wide()
            .chain(Some(0))
            .collect();
        let val: Vec<u16> = std::ffi::OsStr::new(name)
            .encode_wide()
            .chain(Some(0))
            .collect();
        // Expandable strings are read raw (never expanded from the user's environment).
        let flags = RRF_RT_REG_DWORD | RRF_RT_REG_SZ | RRF_RT_REG_MULTI_SZ | RRF_NOEXPAND;
        let mut ty = 0u32;
        let mut bytes = 0u32;
        // SAFETY: size query: both strings are NUL-terminated; a null data pointer with a valid
        // size pointer only writes the needed size and the type.
        let rc = unsafe {
            RegGetValueW(
                hive,
                key.as_ptr(),
                val.as_ptr(),
                flags,
                &mut ty,
                std::ptr::null_mut(),
                &mut bytes,
            )
        };
        if rc != ERROR_SUCCESS || bytes == 0 || bytes > 64 * 1024 {
            return None;
        }
        // u16 backing store (+2 units of slack for a missing terminator).
        let mut buf = vec![0u16; (bytes as usize).div_ceil(2) + 2];
        let mut cap = u32::try_from(buf.len() * 2).ok()?;
        // SAFETY: `buf` holds `cap` writable bytes, 2-byte aligned (a DWORD is read back from
        // its two u16 halves below); the other pointers are as in the size query.
        let rc = unsafe {
            RegGetValueW(
                hive,
                key.as_ptr(),
                val.as_ptr(),
                flags,
                &mut ty,
                buf.as_mut_ptr().cast(),
                &mut cap,
            )
        };
        if rc != ERROR_SUCCESS {
            return None;
        }
        let units = (cap as usize / 2).min(buf.len());
        match ty {
            REG_DWORD if cap >= 4 => {
                let lo = u32::from(buf[0]);
                let hi = u32::from(buf[1]);
                Some(super::RegValue::Dword(lo | (hi << 16)))
            }
            REG_SZ | REG_EXPAND_SZ | REG_MULTI_SZ => {
                let list: Vec<String> = buf[..units]
                    .split(|u| *u == 0)
                    .filter(|s| !s.is_empty())
                    .map(String::from_utf16_lossy)
                    .collect();
                Some(super::RegValue::Strings(list))
            }
            _ => None,
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use std::path::Path;
    pub fn total_ram() -> Option<u64> {
        None
    }
    pub fn physical_cores() -> Option<u32> {
        None
    }
    pub fn free_disk(_: &Path) -> Option<u64> {
        None
    }
    pub fn file_identity(_: &std::fs::File) -> Option<(u32, [u8; 16])> {
        None
    }
    pub fn attach_parent_console() -> bool {
        false
    }
    pub fn program_data() -> Option<std::path::PathBuf> {
        None
    }
    pub fn file_attributes(_: &Path) -> Option<u32> {
        None
    }
    pub fn policy_value(_: bool, _: &str, _: &str) -> Option<super::RegValue> {
        None
    }
}

pub use sr_common::config::policy::RegValue;

/// File attributes by path (`GetFileAttributesW`), without opening the file for data (M5-E: a
/// OneDrive placeholder must not be downloaded just to be looked at).
pub fn file_attributes(path: &Path) -> Option<u32> {
    imp::file_attributes(path)
}

/// Whether `path` is an online-only cloud placeholder (reading it would download it).
pub fn is_online_only(path: &Path) -> bool {
    file_attributes(path).is_some_and(|a| a & sr_sync::fsid::ONLINE_ONLY_ATTRS != 0)
}

/// `%ProgramData%` from the known-folder API (M5-E: where `SeleneRetrieval\policy.toml` lives).
pub fn program_data() -> Option<std::path::PathBuf> {
    imp::program_data()
}

/// One value of `HKLM` (`machine`) or `HKCU` `subkey` (M5-E machine policy). `None` when the key
/// or value is missing or of another type.
pub fn policy_value(machine: bool, subkey: &str, name: &str) -> Option<RegValue> {
    imp::policy_value(machine, subkey, name)
}

/// Installed physical memory in bytes.
pub fn total_ram() -> Option<u64> {
    imp::total_ram()
}

/// Physical cores (not logical processors).
pub fn physical_cores() -> Option<u32> {
    imp::physical_cores()
}

/// Free bytes available to this user on the volume holding `path`.
pub fn free_disk(path: &Path) -> Option<u64> {
    imp::free_disk(path)
}

/// `(volume serial, 128-bit file id)` of an open file.
pub fn file_identity(f: &std::fs::File) -> Option<(u32, [u8; 16])> {
    imp::file_identity(f)
}

/// Attach to the parent's console (hidden `admin` CLI). Returns whether it worked.
pub fn attach_parent_console() -> bool {
    imp::attach_parent_console()
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn hardware_queries_return_plausible_values() {
        let ram = total_ram().unwrap_or(0);
        assert!(ram >= 1 << 30, "ram {ram}");
        let cores = physical_cores().unwrap_or(0);
        let logical = std::thread::available_parallelism().map_or(1, |n| n.get());
        assert!(
            cores >= 1 && cores as usize <= logical,
            "{cores} vs {logical}"
        );
        let free = free_disk(&std::env::temp_dir()).unwrap_or(0);
        assert!(free > 0);
    }

    /// M5-E: the known folder exists; a policy key nobody created reads as absent.
    #[test]
    fn program_data_and_missing_policy_values() {
        let pd = program_data().unwrap_or_default();
        assert!(pd.is_dir(), "{}", pd.display());
        assert_eq!(
            policy_value(true, r"SOFTWARE\Policies\SeleneRetrieval-test-absent", "X"),
            None
        );
        // A value that exists on every Windows install, read through the same path.
        let v = policy_value(
            true,
            r"SOFTWARE\Microsoft\Windows NT\CurrentVersion",
            "CurrentBuild",
        );
        assert!(
            matches!(&v, Some(RegValue::Strings(s)) if !s.is_empty()),
            "{v:?}"
        );
    }

    #[test]
    fn file_identity_is_stable_across_rename() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let a = dir.path().join("a.txt");
        std::fs::write(&a, "x").unwrap_or_else(|e| panic!("{e}"));
        let id1 = std::fs::File::open(&a).ok().and_then(|f| file_identity(&f));
        let b = dir.path().join("b.txt");
        std::fs::rename(&a, &b).unwrap_or_else(|e| panic!("{e}"));
        let id2 = std::fs::File::open(&b).ok().and_then(|f| file_identity(&f));
        assert!(id1.is_some());
        assert_eq!(id1, id2);
    }
}
