//! File identity and cloud-placeholder attributes.
//!
//! * Identity = `(volume serial, 128-bit FILE_ID_INFO.FileId)` (docs/00 §0.A "file IDs"). It is what
//!   rename detection matches on, so a rename keeps `file_id` and therefore chunk IDs. FAT/exFAT
//!   volumes that refuse `FileIdInfo` fall back to the 64-bit `nFileIndex` of
//!   `GetFileInformationByHandle` (zero-extended).
//! * The handle is opened with `FILE_READ_ATTRIBUTES` only, never data access, so an online-only
//!   OneDrive file is not downloaded by being identified.
//! * The catalog stores the **full 64-bit** `FILE_ID_INFO.VolumeSerialNumber` (catalog V3; ReFS
//!   serials use all 64 bits). Catalogs written before V3 hold the low 32 bits; the reconciler
//!   upgrades them in place (`folders::upgrade_volume_serial`).
//! * [`drive_kind`]: `GetDriveTypeW` on the root's drive, so mapped network drive letters are
//!   recognised (not only UNC paths). Network roots are reconcile-only (no watcher).
//!
//! **Reviewed-unsafe module** (workspace lint `unsafe_code = "deny"` is lifted here only): two
//! Win32 calls on an owned handle with correctly sized, zero-initialised out-structs, and
//! `GetDriveTypeW` on a NUL-terminated UTF-16 buffer we own.

use std::io;
use std::path::Path;

/// Identity of a file on its volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileIdentity {
    /// Volume serial number, all 64 bits (catalog `volume_serial`).
    pub volume_serial: u64,
    /// `FILE_ID_INFO.FileId` (catalog `ntfs_file_id`).
    pub file_id: [u8; 16],
}

/// `FILE_ATTRIBUTE_OFFLINE | FILE_ATTRIBUTE_RECALL_ON_OPEN | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`.
pub const ONLINE_ONLY_ATTRS: u32 = 0x0000_1000 | 0x0004_0000 | 0x0040_0000;

/// Whether the metadata describes an online-only (cloud placeholder) file that reading would
/// download.
pub fn is_online_only(md: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        md.file_attributes() & ONLINE_ONLY_ATTRS != 0
    }
    #[cfg(not(windows))]
    {
        let _ = md;
        false
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod imp {
    use std::fs::OpenOptions;
    use std::io;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;

    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_INFO, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileIdInfo,
        GetFileInformationByHandle, GetFileInformationByHandleEx,
    };

    use super::FileIdentity;

    pub fn identity(path: &Path) -> io::Result<FileIdentity> {
        let f = OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            // Needed to open directories (folder roots); harmless for files.
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?;
        let h = f.as_raw_handle();
        // SAFETY: `h` is a valid handle owned by `f` for the duration of the calls; the out
        // structs are plain-old-data, zero-initialised and passed with their exact size.
        let mut info: FILE_ID_INFO = unsafe { std::mem::zeroed() };
        let size = u32::try_from(std::mem::size_of::<FILE_ID_INFO>()).unwrap_or(u32::MAX);
        let ok =
            unsafe { GetFileInformationByHandleEx(h, FileIdInfo, (&raw mut info).cast(), size) };
        if ok != 0 {
            return Ok(FileIdentity {
                volume_serial: info.VolumeSerialNumber,
                file_id: info.FileId.Identifier,
            });
        }
        // FAT/exFAT and some network file systems: 64-bit index only.
        let mut bh: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: as above.
        let ok = unsafe { GetFileInformationByHandle(h, &raw mut bh) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let idx = (u64::from(bh.nFileIndexHigh) << 32) | u64::from(bh.nFileIndexLow);
        let mut file_id = [0u8; 16];
        file_id[..8].copy_from_slice(&idx.to_le_bytes());
        Ok(FileIdentity {
            volume_serial: u64::from(bh.dwVolumeSerialNumber),
            file_id,
        })
    }

    /// `GetDriveTypeW` for the drive letter of `root` (`C:\…` or `\\?\C:\…`).
    pub fn drive_type(root: &Path) -> Option<u32> {
        let s = root.as_os_str().to_string_lossy();
        let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
        let b = s.as_bytes();
        if b.len() < 2 || b[1] != b':' || !b[0].is_ascii_alphabetic() {
            return None;
        }
        let wide: Vec<u16> = [u16::from(b[0]), u16::from(b':'), u16::from(b'\\'), 0].to_vec();
        // SAFETY: `wide` is a NUL-terminated UTF-16 string ("X:\") that outlives the call; the
        // function only reads it.
        Some(unsafe { windows_sys::Win32::Storage::FileSystem::GetDriveTypeW(wide.as_ptr()) })
    }
}

#[cfg(not(windows))]
mod imp {
    use std::io;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;

    use super::FileIdentity;

    pub fn identity(path: &Path) -> io::Result<FileIdentity> {
        let md = std::fs::metadata(path)?;
        let mut file_id = [0u8; 16];
        file_id[..8].copy_from_slice(&md.ino().to_le_bytes());
        Ok(FileIdentity {
            volume_serial: md.dev(),
            file_id,
        })
    }

    pub fn drive_type(_root: &Path) -> Option<u32> {
        None
    }
}

/// Where a folder root lives, as far as the watcher is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriveKind {
    /// Fixed disk, RAM disk, optical.
    Local,
    /// USB stick, SD card: watched; the root probe notices when it goes away.
    Removable,
    /// UNC share or a mapped network drive letter: reconcile-only.
    Network,
    /// Could not tell (treated like local).
    Unknown,
}

// `DRIVE_*` live in windows-sys' `Win32_System_WindowsProgramming` feature, which the workspace
// doesn't enable; the values are fixed by the Win32 ABI.
const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_FIXED: u32 = 3;
const DRIVE_REMOTE: u32 = 4;
const DRIVE_CDROM: u32 = 5;
const DRIVE_RAMDISK: u32 = 6;

/// Whether a path is a UNC network path (`\\server\share`, `\\?\UNC\…`).
pub fn is_unc(root: &Path) -> bool {
    let s = root.to_string_lossy();
    let lower = s.to_ascii_lowercase();
    lower.starts_with(r"\\?\unc\")
        || (s.starts_with(r"\\") && !s.starts_with(r"\\?\") && !s.starts_with(r"\\.\"))
}

/// Classifies the drive a root lives on: UNC prefix, else `GetDriveTypeW` on its drive letter
/// (which also recognises mapped network drives).
pub fn drive_kind(root: &Path) -> DriveKind {
    if is_unc(root) {
        return DriveKind::Network;
    }
    match imp::drive_type(root) {
        Some(DRIVE_REMOTE) => DriveKind::Network,
        Some(DRIVE_REMOVABLE) => DriveKind::Removable,
        Some(DRIVE_FIXED | DRIVE_CDROM | DRIVE_RAMDISK) => DriveKind::Local,
        _ => DriveKind::Unknown,
    }
}

/// Identity of `path` (file or directory). Follows a final symlink (the walker only asks for
/// entries it decided to follow).
pub fn identity(path: &Path) -> io::Result<FileIdentity> {
    imp::identity(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_stable_across_rename_and_differs_between_files() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("a.txt");
        let b = d.path().join("b.txt");
        std::fs::write(&a, "a").unwrap();
        std::fs::write(&b, "b").unwrap();
        let ia = identity(&a).unwrap();
        let ib = identity(&b).unwrap();
        assert_ne!(ia.file_id, ib.file_id);
        assert_eq!(ia.volume_serial, ib.volume_serial);
        let moved = d.path().join("moved.txt");
        std::fs::rename(&a, &moved).unwrap();
        assert_eq!(identity(&moved).unwrap(), ia);
        assert!(identity(d.path()).is_ok(), "directories (roots) too");
        let md = std::fs::metadata(&b).unwrap();
        assert!(!is_online_only(&md));
    }

    #[test]
    fn drive_kinds() {
        assert_eq!(
            drive_kind(Path::new(r"\\server\share\x")),
            DriveKind::Network
        );
        assert_eq!(
            drive_kind(Path::new(r"\\?\UNC\server\share")),
            DriveKind::Network
        );
        assert!(!is_unc(Path::new(r"\\?\C:\x")));
        assert!(!is_unc(Path::new(r"\\.\pipe\x")));
        let d = tempfile::tempdir().unwrap();
        if cfg!(windows) {
            assert!(
                matches!(drive_kind(d.path()), DriveKind::Local),
                "temp dir is on a fixed disk: {:?}",
                drive_kind(d.path())
            );
            assert_eq!(drive_kind(Path::new("relative")), DriveKind::Unknown);
        }
    }
}
