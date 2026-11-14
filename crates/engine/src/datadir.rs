//! The data directory: a short, plain local path (Amendment 3.12) owned by the daemon alone, with a
//! protected DACL granting only the current user and SYSTEM (docs/00 §0.A Q6, §2.6 "Data at rest").
//!
//! The ACL is set with `icacls.exe` from System32 (absolute path, never `PATH`), so the engine needs
//! no extra `unsafe`. It is idempotent and runs at every start, before `host.lock` is created, so
//! every file the daemon writes afterwards inherits it.
//!
//! Unix (macOS port): the folder must be a real folder (never a link) owned by this uid; it is set
//! to mode 0700, its extended ACL (macOS) is removed, and it is checked again, so nothing below it
//! is reachable by other accounts.

use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::process::Command;

use sr_common::error::{Error, ErrorCode, Section};

/// Sub-paths of the data directory.
///
/// M4 (page 32): the **anchor** (`%LOCALAPPDATA%\SeleneRetrieval`, or `--data-dir`) names the
/// pipe and the scheduled task and holds `settings.toml`, `logs\`, the lock files and the
/// pointer [`INDEX_POINTER`]. The **index root** (`root`) holds the search index: `catalog.db`,
/// `lance\`, `tantivy\` and downloaded `models\`. Both are the same folder unless the index was
/// moved to another drive.
#[derive(Debug, Clone)]
pub struct DataLayout {
    /// The index root (catalog, stores, models).
    pub root: PathBuf,
    /// The anchor data dir (settings, logs, locks, the index pointer).
    pub anchor: PathBuf,
    pub catalog: PathBuf,
    pub lance: PathBuf,
    pub tantivy: PathBuf,
    pub settings: PathBuf,
    pub logs: PathBuf,
}

/// Pointer file in the anchor that names a moved index root (absent = the anchor itself).
pub const INDEX_POINTER: &str = "index-location.json";

/// Everything that belongs to the search index (moved together, deleted together).
pub const INDEX_ENTRIES: &[&str] = &[
    "catalog.db",
    "catalog.db-wal",
    "catalog.db-shm",
    "lance",
    "tantivy",
    "models",
];

impl DataLayout {
    /// Index and anchor in one folder (no move).
    pub fn new(root: &Path) -> Self {
        Self::with_index(root, root)
    }

    /// Anchor `anchor`, index at `index`.
    pub fn with_index(anchor: &Path, index: &Path) -> Self {
        Self {
            root: index.to_path_buf(),
            anchor: anchor.to_path_buf(),
            catalog: index.join("catalog.db"),
            lance: index.join("lance"),
            tantivy: index.join("tantivy"),
            settings: anchor.join(sr_common::config::SETTINGS_FILE),
            logs: anchor.join("logs"),
        }
    }

    /// Whether the index lives in the anchor (the default location).
    pub fn is_default(&self) -> bool {
        sr_client::naming::data_dir_key(&self.root) == sr_client::naming::data_dir_key(&self.anchor)
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Pointer {
    path: PathBuf,
}

/// The moved index root named by the anchor's pointer file, if any.
pub fn read_pointer(anchor: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(anchor.join(INDEX_POINTER)).ok()?;
    serde_json::from_str::<Pointer>(&text).ok().map(|p| p.path)
}

/// Write (or with `None` remove) the pointer atomically: temp file + rename, so a crash leaves the
/// old or the new pointer, never half of one.
pub fn write_pointer(anchor: &Path, index: Option<&Path>) -> Result<(), Error> {
    let path = anchor.join(INDEX_POINTER);
    match index {
        None => match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::from(e).with_detail("removing the index pointer")),
        },
        Some(p) => {
            let tmp = anchor.join(format!("{INDEX_POINTER}.tmp"));
            let body = serde_json::to_vec_pretty(&Pointer {
                path: p.to_path_buf(),
            })
            .map_err(|e| Error::new(ErrorCode::Internal).with_source(e))?;
            write_synced(&tmp, &body)?;
            std::fs::rename(&tmp, &path)
                .map_err(|e| Error::from(e).with_detail("switching the index pointer"))
        }
    }
}

/// Write a file and flush it to disk.
pub(crate) fn write_synced(path: &Path, body: &[u8]) -> Result<(), Error> {
    use std::io::Write;
    let mut f = std::fs::File::create(path)
        .map_err(|e| Error::from(e).with_detail(format!("creating {}", path.display())))?;
    f.write_all(body)
        .and_then(|()| f.sync_all())
        .map_err(|e| Error::from(e).with_detail(format!("writing {}", path.display())))
}

fn settings_invalid(detail: impl Into<String>) -> Error {
    Error::new(ErrorCode::SettingsInvalid)
        .with_section(Section::System)
        .with_detail(detail)
}

/// Validate one root (client rule + the stricter store-root rule of sr-core).
pub fn validate_index_root(root: &Path) -> Result<(), Error> {
    sr_client::naming::validate_data_dir(root)?;
    // Lance appends ~100 chars below its root; sr-core enforces ≤ 150 for the store roots.
    sr_core::index::datadir::validate_store_root(&root.join("lance"))?;
    sr_core::index::datadir::validate_store_root(&root.join("tantivy"))?;
    Ok(())
}

/// Validate the anchor data dir, resolve the index root (pointer file, page 32) and create both.
///
/// * A pointer that breaks the path rule (hand-edited) is ignored with a warning.
/// * A pointer to a folder that no longer exists (the drive is gone) refuses to start with
///   `IndexMissing` (R34, M5-E: "Reconnect the drive, or move the index back"): starting on an
///   empty index in the anchor would show "No folders yet" and later fork the user's index.
///   `sr-daemon` then waits in its recovery mode ([`crate::recover`]).
pub fn prepare(anchor: &Path) -> Result<DataLayout, Error> {
    validate_index_root(anchor)?;
    let index = match read_pointer(anchor) {
        Some(p) if validate_index_root(&p).is_err() => {
            tracing::warn!(target: "sr_engine::datadir", path = %p.display(), "index pointer breaks the path rule; ignored");
            anchor.to_path_buf()
        }
        Some(p) if !p.is_dir() => {
            return Err(Error::new(ErrorCode::IndexMissing)
                .with_name(p.display().to_string())
                .with_detail("the moved search index folder is missing"));
        }
        Some(p) => p,
        None => anchor.to_path_buf(),
    };
    let layout = DataLayout::with_index(anchor, &index);
    for dir in [anchor, index.as_path()] {
        std::fs::create_dir_all(dir).map_err(|e| {
            Error::from(e).with_detail(format!("creating data directory {}", dir.display()))
        })?;
        if !dir.is_dir() {
            return Err(settings_invalid("data directory is not a directory"));
        }
    }
    // S-02 (R39): a moved index folder is re-checked at every start (owner ours, DACL private).
    if index.as_path() != anchor {
        protect_index_dir(&index)?;
    }
    Ok(layout)
}

/// `icacls.exe` from System32.
#[cfg(windows)]
fn icacls() -> Result<PathBuf, Error> {
    let sys =
        sr_client::win::system_dir().map_err(|e| Error::new(ErrorCode::Internal).with_source(e))?;
    Ok(sys.join("icacls.exe"))
}

/// Protected DACL: current user and SYSTEM, full control, inherited by everything below; no
/// inherited ACEs from `%LOCALAPPDATA%`. Idempotent.
#[cfg(windows)]
pub fn restrict_acl(root: &Path) -> Result<(), Error> {
    let sid = sr_client::win::user_sid_string()
        .map_err(|e| Error::new(ErrorCode::Internal).with_source(e))?;
    let out = Command::new(icacls()?)
        .arg(root)
        .arg("/inheritance:r")
        .arg("/grant:r")
        .arg(format!("*{sid}:(OI)(CI)F"))
        .arg("/grant:r")
        .arg("*S-1-5-18:(OI)(CI)F")
        .arg("/Q")
        .output()
        .map_err(|e| Error::new(ErrorCode::Internal).with_source(e))?;
    if !out.status.success() {
        return Err(Error::new(ErrorCode::Internal).with_detail(format!(
            "icacls failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

/// Is `owner` acceptable as the owner of a folder that will hold the index? Us, or
/// `BUILTIN\Administrators` (folders an elevated process of ours created). Pure, for tests.
/// SDDL prints Administrators as the alias `BA`.
#[cfg(windows)]
pub fn owner_is_acceptable(owner: &str, me: &str) -> bool {
    owner.eq_ignore_ascii_case(me)
        || owner.eq_ignore_ascii_case("BA")
        || owner.eq_ignore_ascii_case(sr_client::win::ADMINISTRATORS_SID)
}

/// Is the `D:` part of `sddl` a protected DACL whose ACEs are all allow ACEs for `me` or SYSTEM
/// (`SY` / `S-1-5-18`)? Pure, for tests. Anything unexpected (deny ACEs, other trustees, an
/// unprotected DACL, no DACL) is "not private".
#[cfg(windows)]
pub fn dacl_is_private(sddl: &str, me: &str) -> bool {
    let Some(d) = sddl.find("D:").map(|i| &sddl[i + 2..]) else {
        return false;
    };
    // The DACL ends where the SACL ("S:") would start, outside any ACE.
    let flags_end = d.find('(').unwrap_or(d.len());
    if !d[..flags_end].contains('P') {
        return false;
    }
    let mut aces = 0;
    let mut rest = &d[flags_end..];
    while let Some(r) = rest.strip_prefix('(') {
        let Some(end) = r.find(')') else {
            return false;
        };
        let fields: Vec<&str> = r[..end].split(';').collect();
        let (Some(kind), Some(trustee)) = (fields.first(), fields.last()) else {
            return false;
        };
        let trustee_ok = trustee.eq_ignore_ascii_case(me)
            || trustee.eq_ignore_ascii_case("SY")
            || trustee.eq_ignore_ascii_case("S-1-5-18");
        if *kind != "A" || !trustee_ok {
            return false;
        }
        aces += 1;
        rest = &r[end + 1..];
    }
    aces > 0 && (rest.is_empty() || rest.starts_with("S:"))
}

/// The `O:` part of an SDDL string (`O:<sid or alias>G:…D:…`). SIDs and aliases contain no
/// `:`, so the owner ends at the first of `G:` / `D:` / `S:`.
#[cfg(windows)]
pub fn owner_of(sddl: &str) -> Option<&str> {
    let o = sddl.strip_prefix("O:")?;
    let end = ["G:", "D:", "S:"]
        .iter()
        .filter_map(|m| o.find(m))
        .min()
        .unwrap_or(o.len());
    Some(&o[..end])
}

/// M5-E S-02 (R39): make a (moved) index folder exclusively ours, whoever pre-created it.
///
/// 1. Never a link (junction / symlink): judged by itself, never followed.
/// 2. The owner must be us or Administrators. Another account's folder is refused with
///    `PermissionDenied`: as owner it keeps `WRITE_DAC` and could re-grant itself at any time.
/// 3. Unless the DACL is already private (protected, only us + SYSTEM), set owner (if it was
///    Administrators) and a protected two-ACE DACL in **one** call
///    ([`sr_client::win::set_private_dacl`]). `icacls /grant:r` alone kept foreign explicit
///    ACEs. Runs before the first file is copied, on resume, and at every start with a pointer.
#[cfg(windows)]
pub fn protect_index_dir(dir: &Path) -> Result<(), Error> {
    let denied = |detail: String| {
        Error::new(ErrorCode::PermissionDenied)
            .with_name(dir.display().to_string())
            .with_detail(detail)
    };
    if dir.is_symlink() {
        return Err(denied("the index folder is a link".into()));
    }
    let me = sr_client::win::user_sid_string()
        .map_err(|e| Error::new(ErrorCode::Internal).with_source(e))?;
    let sddl = sr_client::win::path_security_sddl(dir)
        .map_err(|e| Error::from(e).with_detail("reading the index folder's security"))?;
    let owner = owner_of(&sddl).unwrap_or_default();
    // SDDL may print an account as an alias (e.g. "LA" for the built-in Administrator).
    let owner = sr_client::win::canonical_sid(owner).unwrap_or_else(|_| owner.to_string());
    if !owner_is_acceptable(&owner, &me) {
        return Err(denied(format!(
            "the index folder belongs to another account ({owner})"
        )));
    }
    let owner_is_me = owner.eq_ignore_ascii_case(&me);
    if owner_is_me && dacl_is_private(&sddl, &me) {
        return Ok(());
    }
    sr_client::win::set_private_dacl(dir, &me, !owner_is_me)
        .map_err(|e| Error::from(e).with_detail("protecting the index folder"))
}

/// The data dir's ACL as `icacls` prints it (diagnostics and tests).
#[cfg(windows)]
pub fn describe_acl(root: &Path) -> Result<String, Error> {
    let out = Command::new(icacls()?)
        .arg(root)
        .output()
        .map_err(|e| Error::new(ErrorCode::Internal).with_source(e))?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Unix: make `dir` private to this account, or refuse. `lstat` (a link is refused, never
/// followed), owner must be `getuid()`, then mode 0700 and (macOS) no extended ACL, set through
/// an `O_NOFOLLOW` handle ([`sr_client::sys::make_private_dir`]), and a second check that must
/// show no group or other bits and no ACL. An inherited ACL is removed, not refused. Idempotent.
/// Another account's folder is `PermissionDenied`.
#[cfg(unix)]
fn make_private_dir(dir: &Path) -> Result<(), Error> {
    use std::os::unix::fs::MetadataExt;
    let denied = |detail: String| {
        Error::new(ErrorCode::PermissionDenied)
            .with_name(dir.display().to_string())
            .with_detail(detail)
    };
    let me = sr_client::sys::uid();
    let m = std::fs::symlink_metadata(dir)
        .map_err(|e| Error::from(e).with_detail("reading the data folder's owner"))?;
    if m.file_type().is_symlink() {
        return Err(denied("the folder is a link".into()));
    }
    if !m.is_dir() {
        return Err(settings_invalid("data directory is not a directory"));
    }
    if m.uid() != me {
        return Err(denied(format!(
            "the folder belongs to another account (uid {})",
            m.uid()
        )));
    }
    sr_client::sys::make_private_dir(dir, me).map_err(|e| {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            denied(format!("the folder could not be made private: {e}"))
        } else {
            Error::from(e).with_detail("protecting the data folder")
        }
    })
}

/// Unix twin of the protected DACL: the data dir is ours and mode 0700 ([`make_private_dir`]).
#[cfg(unix)]
pub fn restrict_acl(root: &Path) -> Result<(), Error> {
    make_private_dir(root)
}

/// Unix twin of S-02: a (moved) index folder must be ours and is made mode 0700; a link or
/// another account's folder is refused.
#[cfg(unix)]
pub fn protect_index_dir(dir: &Path) -> Result<(), Error> {
    make_private_dir(dir)
}

/// Unix: owner and mode of `root` (diagnostics and tests), e.g. `uid=501 mode=700`.
#[cfg(unix)]
pub fn describe_acl(root: &Path) -> Result<String, Error> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::symlink_metadata(root).map_err(Error::from)?;
    Ok(format!("uid={} mode={:o}", m.uid(), m.mode() & 0o7777))
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn rejects_unc_verbatim_and_long_paths() {
        for bad in [
            r"\\server\share\sr",
            r"\\?\C:\sr",
            "relative\\sr",
            &format!(r"C:\{}", "x".repeat(160)),
        ] {
            let e = prepare(Path::new(bad)).err();
            assert_eq!(
                e.map(|e| e.code),
                Some(ErrorCode::SettingsInvalid),
                "{bad} should be refused"
            );
        }
    }

    #[test]
    fn sddl_owner_and_private_dacl() {
        let me = "S-1-5-21-1-2-3-1001";
        let s = format!("O:{me}G:{me}D:P(A;OICI;FA;;;{me})(A;OICI;FA;;;SY)");
        assert_eq!(owner_of(&s), Some(me));
        assert_eq!(owner_of("O:BAD:P(A;;FA;;;SY)"), Some("BA"));
        assert_eq!(owner_of("G:SY"), None);
        assert!(dacl_is_private(&s, me));
        assert!(dacl_is_private(
            &format!("O:BAD:PAI(A;OICI;FA;;;{me})S:(ML;;NW;;;LW)"),
            me
        ));
        // Not protected, foreign trustee, deny ACE, no DACL, empty DACL.
        assert!(!dacl_is_private(&format!("D:(A;OICI;FA;;;{me})"), me));
        assert!(!dacl_is_private(
            &format!("D:P(A;OICI;FA;;;{me})(A;OICI;FA;;;BU)"),
            me
        ));
        assert!(!dacl_is_private(
            &format!("D:P(A;OICI;FA;;;{me})(D;;FA;;;SY)"),
            me
        ));
        assert!(!dacl_is_private(&format!("O:{me}"), me));
        assert!(!dacl_is_private("D:P", me));
        assert!(!dacl_is_private(&format!("D:P(A;OICI;FA;;;{me}"), me));
        assert!(owner_is_acceptable(me, me));
        assert!(owner_is_acceptable("BA", me));
        assert!(owner_is_acceptable(sr_client::win::ADMINISTRATORS_SID, me));
        assert!(!owner_is_acceptable("S-1-5-21-9-9-9-1002", me));
        assert!(!owner_is_acceptable("SY", me));
    }

    #[test]
    fn protect_index_dir_drops_foreign_aces() {
        let dir = tempfile::tempdir().unwrap();
        let idx = dir.path().join("moved");
        std::fs::create_dir(&idx).unwrap();
        // Pre-existing folder with an explicit grant for Users (what a squatter would leave).
        let ok = Command::new(icacls().unwrap())
            .arg(&idx)
            .arg("/grant")
            .arg("*S-1-5-32-545:(OI)(CI)F")
            .arg("/Q")
            .status()
            .unwrap();
        assert!(ok.success());
        protect_index_dir(&idx).unwrap();
        protect_index_dir(&idx).unwrap(); // idempotent (fast path)
        let me = sr_client::win::user_sid_string().unwrap();
        let sddl = sr_client::win::path_security_sddl(&idx).unwrap();
        assert!(dacl_is_private(&sddl, &me), "{sddl}");
        let acl = describe_acl(&idx).unwrap();
        for other in ["BUILTIN\\Users", "Everyone", "(I)"] {
            assert!(!acl.contains(other), "{other} in {acl}");
        }
    }

    #[test]
    fn protect_index_dir_refuses_links() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        // Directory symlinks need a privilege; junctions don't. Skip if neither is possible.
        let made = Command::new(sr_client::win::system_dir().unwrap().join("cmd.exe"))
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&real)
            .output()
            .is_ok_and(|o| o.status.success());
        if !made {
            return;
        }
        let e = protect_index_dir(&link).unwrap_err();
        assert_eq!(e.code, ErrorCode::PermissionDenied);
    }

    #[test]
    fn acl_is_protected_user_and_system_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sr");
        prepare(&root).unwrap();
        restrict_acl(&root).unwrap();
        restrict_acl(&root).unwrap(); // idempotent
        let acl = describe_acl(&root).unwrap();
        // No inherited ACEs ("(I)") and nothing for Users / Everyone / Administrators.
        assert!(!acl.contains("(I)"), "{acl}");
        for other in ["BUILTIN\\Users", "Everyone", "BUILTIN\\Administrators"] {
            assert!(!acl.contains(other), "{other} in {acl}");
        }
        assert!(acl.contains("NT AUTHORITY\\SYSTEM"), "{acl}");
        // A file created afterwards inherits the protected ACL only.
        std::fs::write(root.join("f"), "x").unwrap();
        let f = describe_acl(&root.join("f")).unwrap();
        assert!(
            !f.contains("BUILTIN\\Users") && !f.contains("Everyone"),
            "{f}"
        );
    }
}

#[cfg(all(test, unix))]
mod unix_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn data_dir_is_made_private_and_links_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sr");
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        restrict_acl(&root).unwrap();
        restrict_acl(&root).unwrap(); // idempotent
        assert_eq!(
            describe_acl(&root).unwrap(),
            format!("uid={} mode=700", sr_client::sys::uid())
        );
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&root, &link).unwrap();
        let e = protect_index_dir(&link).unwrap_err();
        assert_eq!(e.code, ErrorCode::PermissionDenied);
    }

    /// macOS: an inherited `everyone allow` ACL is removed, not just the mode bits fixed.
    #[cfg(target_os = "macos")]
    #[test]
    fn data_dir_loses_its_extended_acl() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sr");
        std::fs::create_dir(&root).unwrap();
        let ok = std::process::Command::new("/bin/chmod")
            .arg("+a")
            .arg("everyone allow list,search")
            .arg(&root)
            .status()
            .unwrap();
        assert!(ok.success());
        assert_eq!(sr_client::sys::extended_acl_entries(&root).unwrap(), (1, 1));
        restrict_acl(&root).unwrap();
        assert_eq!(sr_client::sys::extended_acl_entries(&root).unwrap(), (0, 0));
        assert_eq!(
            describe_acl(&root).unwrap(),
            format!("uid={} mode=700", sr_client::sys::uid())
        );
        // An index folder too, also when it is already mode 0700.
        let idx = dir.path().join("idx");
        std::fs::create_dir(&idx).unwrap();
        std::fs::set_permissions(&idx, std::fs::Permissions::from_mode(0o700)).unwrap();
        let ok = std::process::Command::new("/bin/chmod")
            .arg("+a")
            .arg("group:staff allow list,search,read")
            .arg(&idx)
            .status()
            .unwrap();
        assert!(ok.success());
        protect_index_dir(&idx).unwrap();
        assert_eq!(sr_client::sys::extended_acl_entries(&idx).unwrap(), (0, 0));
    }
}
