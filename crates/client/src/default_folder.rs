//! "Create a folder for me" (Amendment 13, `docs/07-default-folder-plan.md`): where the default
//! folder goes and how it is made. Std only (sr-bridge links this crate; no engine stack).
//!
//! The caller passes the Documents folder: the desktop app resolves it with the known-folder API
//! (`FOLDERID_Documents`, so OneDrive / folder redirection is followed); tests pass a temp folder.
//! The folder is made by the **app**, as the user; the background service only ever reads it.
//!
//! Rules:
//! * the folder is `Selene Retrieval` directly inside Documents; when that name is taken by a
//!   file or a link (symlink / junction: never followed), `Selene Retrieval (2)` … `(20)`;
//! * an existing folder with the name is used as it is (never wiped, renamed or moved);
//! * Documents itself is never created (`create_dir`, not `create_dir_all`);
//! * every check is plain path logic or one metadata call; nothing is deleted here.

use std::io;
use std::path::{Path, PathBuf};

/// The folder's name in Documents (the product name, as in the Start menu).
pub const FOLDER_NAME: &str = "Selene Retrieval";
/// Highest number tried after the plain name (`Selene Retrieval (20)`).
pub const MAX_NUMBER: u32 = 20;

/// `Selene Retrieval`, `Selene Retrieval (2)`, …, `Selene Retrieval (20)`, in that order.
pub fn names() -> impl Iterator<Item = String> {
    std::iter::once(FOLDER_NAME.to_owned())
        .chain((2..=MAX_NUMBER).map(|n| format!("{FOLDER_NAME} ({n})")))
}

/// Whether `name` is one of [`names`] (case-insensitive, like Windows).
pub fn is_default_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    let base = FOLDER_NAME.to_lowercase();
    if lower == base {
        return true;
    }
    lower
        .strip_prefix(&base)
        .and_then(|r| r.strip_prefix(" ("))
        .and_then(|r| r.strip_suffix(')'))
        .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) && !n.starts_with('0'))
        .and_then(|n| n.parse::<u32>().ok())
        .is_some_and(|n| (2..=MAX_NUMBER).contains(&n))
}

/// Lower-cased, `\`-separated, without the verbatim prefix or a trailing separator (Windows
/// paths compare case-insensitively).
fn key(p: &Path) -> String {
    let s = p.to_string_lossy().replace('/', "\\");
    let s = if let Some(r) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{r}")
    } else if let Some(r) = s.strip_prefix(r"\\?\") {
        r.to_owned()
    } else {
        s
    };
    s.trim_end_matches('\\').to_lowercase()
}

/// Whether `path` is the default folder (or a numbered one) directly inside `docs`.
pub fn is_default_folder(docs: &Path, path: &Path) -> bool {
    let (d, p) = (key(docs), key(path));
    !d.is_empty()
        && p.strip_prefix(&d)
            .and_then(|r| r.strip_prefix('\\'))
            .is_some_and(|name| !name.contains('\\') && is_default_name(name))
}

/// Whether `path` is `root` or inside it (plain path comparison; no file-system access).
pub fn is_within(path: &Path, root: &Path) -> bool {
    let (p, r) = (key(path), key(root));
    !r.is_empty()
        && (p == r
            || p.strip_prefix(&r)
                .is_some_and(|rest| rest.starts_with('\\')))
}

/// The folder [`make`] returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Made {
    pub path: PathBuf,
    /// `true` = made now; `false` = it was already there (used as it is).
    pub created: bool,
}

/// Why no folder could be made (each has its own plain message in the app).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MakeError {
    /// Documents doesn't exist or can't be reached.
    DocumentsMissing,
    /// Windows refused (permissions, or ransomware protection / Controlled Folder Access).
    AccessDenied,
    /// The drive is full (or the user's quota is used up).
    DiskFull,
    /// Every name up to `(MAX_NUMBER)` is taken by a file or a link.
    NoFreeName,
    /// Anything else.
    Other,
}

const ERROR_FILE_NOT_FOUND: i32 = 2;
const ERROR_PATH_NOT_FOUND: i32 = 3;
const ERROR_ACCESS_DENIED: i32 = 5;
const ERROR_WRITE_PROTECT: i32 = 19;
const ERROR_NOT_READY: i32 = 21;
const ERROR_HANDLE_DISK_FULL: i32 = 39;
const ERROR_BAD_NETPATH: i32 = 53;
const ERROR_BAD_NET_NAME: i32 = 67;
const ERROR_DISK_FULL: i32 = 112;
const ERROR_DISK_QUOTA_EXCEEDED: i32 = 1295;

/// Plain class of an OS error from a metadata or create call.
pub fn classify(e: &io::Error) -> MakeError {
    match e.raw_os_error() {
        // Controlled Folder Access answers a blocked write with "access denied" too.
        Some(ERROR_ACCESS_DENIED | ERROR_WRITE_PROTECT) => MakeError::AccessDenied,
        Some(ERROR_DISK_FULL | ERROR_HANDLE_DISK_FULL | ERROR_DISK_QUOTA_EXCEEDED) => {
            MakeError::DiskFull
        }
        Some(
            ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND | ERROR_NOT_READY | ERROR_BAD_NETPATH
            | ERROR_BAD_NET_NAME,
        ) => MakeError::DocumentsMissing,
        _ => match e.kind() {
            io::ErrorKind::PermissionDenied => MakeError::AccessDenied,
            io::ErrorKind::StorageFull => MakeError::DiskFull,
            io::ErrorKind::NotFound => MakeError::DocumentsMissing,
            _ => MakeError::Other,
        },
    }
}

/// Finds or makes the default folder in `docs` (see the module rules). Blocking: the app runs it
/// off the UI thread with a time limit (a network Documents folder can hang).
pub fn make(docs: &Path) -> Result<Made, MakeError> {
    if !docs.is_absolute() {
        return Err(MakeError::DocumentsMissing);
    }
    match std::fs::metadata(docs) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => return Err(MakeError::DocumentsMissing),
        Err(e) => {
            return Err(match classify(&e) {
                MakeError::AccessDenied => MakeError::AccessDenied,
                _ => MakeError::DocumentsMissing,
            });
        }
    }
    // A second look covers a folder made at the same moment by someone else.
    for _ in 0..3 {
        let mut free = None;
        for name in names() {
            let p = docs.join(&name);
            match std::fs::symlink_metadata(&p) {
                // A real folder (not a link or junction): use it as it is.
                Ok(m) if m.is_dir() && !m.file_type().is_symlink() => {
                    return Ok(Made {
                        path: p,
                        created: false,
                    });
                }
                // A file, a symlink or a junction: taken, try the next name.
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    free = Some(p);
                    break;
                }
                Err(e) => return Err(classify(&e)),
            }
        }
        let Some(p) = free else {
            return Err(MakeError::NoFreeName);
        };
        match std::fs::create_dir(&p) {
            Ok(()) => {
                return Ok(Made {
                    path: p,
                    created: true,
                });
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(classify(&e)),
        }
    }
    Err(MakeError::Other)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "sr-df-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&d).unwrap_or_else(|e| panic!("{e}"));
        d
    }

    #[test]
    fn names_and_numbering() {
        let n: Vec<String> = names().collect();
        assert_eq!(n.len(), 20);
        assert_eq!(n[0], "Selene Retrieval");
        assert_eq!(n[1], "Selene Retrieval (2)");
        assert_eq!(n[19], "Selene Retrieval (20)");
        for ok in [
            "Selene Retrieval",
            "selene retrieval",
            "SELENE RETRIEVAL (2)",
            "Selene Retrieval (20)",
        ] {
            assert!(is_default_name(ok), "{ok}");
        }
        for no in [
            "Selene Retrieval (1)",
            "Selene Retrieval (21)",
            "Selene Retrieval (02)",
            "Selene Retrieval (+2)",
            "Selene Retrieval ()",
            "Selene Retrieval Old",
            "Selene Retrieval(2)",
            "SeleneRetrieval",
            "",
        ] {
            assert!(!is_default_name(no), "{no}");
        }
    }

    #[test]
    fn default_folder_and_within_are_plain_path_checks() {
        let docs = Path::new(r"C:\Users\Ana\Documents");
        assert!(is_default_folder(
            docs,
            Path::new(r"C:\Users\Ana\Documents\Selene Retrieval")
        ));
        assert!(is_default_folder(
            docs,
            Path::new(r"c:\users\ana\documents\selene retrieval\")
        ));
        assert!(is_default_folder(
            docs,
            Path::new(r"\\?\C:\Users\Ana\Documents\Selene Retrieval (3)")
        ));
        assert!(is_default_folder(
            Path::new(r"C:\Users\Ana\Documents\"),
            Path::new("C:/Users/Ana/Documents/Selene Retrieval")
        ));
        assert!(!is_default_folder(
            docs,
            Path::new(r"C:\Users\Ana\Documents\Selene Retrieval Old")
        ));
        assert!(!is_default_folder(
            docs,
            Path::new(r"C:\Users\Ana\Documents\Selene Retrieval\sub")
        ));
        assert!(!is_default_folder(
            docs,
            Path::new(r"C:\Users\Ana\DocumentsSelene Retrieval")
        ));
        assert!(!is_default_folder(docs, docs));
        assert!(!is_default_folder(
            Path::new(""),
            Path::new(r"\Selene Retrieval")
        ));

        let target = docs.join(FOLDER_NAME);
        assert!(is_within(&target, docs));
        assert!(is_within(&target, Path::new(r"C:\Users\Ana")));
        assert!(is_within(&target, &target));
        assert!(!is_within(&target, Path::new(r"C:\Users\Ana\Doc")));
        assert!(!is_within(docs, &target));
        let unc = Path::new(r"\\nas\home\ana\Documents");
        assert!(is_within(
            &unc.join(FOLDER_NAME),
            Path::new(r"\\?\UNC\nas\home\ana")
        ));
    }

    #[test]
    fn make_creates_once_then_reuses_and_never_touches_contents() {
        let docs = tmp();
        let first = make(&docs).unwrap_or_else(|e| panic!("{e:?}"));
        assert!(first.created);
        assert_eq!(first.path, docs.join(FOLDER_NAME));
        assert!(first.path.is_dir());
        std::fs::write(first.path.join("note.txt"), "mine").unwrap_or_else(|e| panic!("{e}"));
        let again = make(&docs).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(
            again,
            Made {
                path: first.path.clone(),
                created: false
            }
        );
        assert_eq!(
            std::fs::read_to_string(first.path.join("note.txt")).unwrap_or_default(),
            "mine",
            "an existing folder is used as it is"
        );
        let _ = std::fs::remove_dir_all(&docs);
    }

    #[test]
    fn a_file_with_the_name_gets_a_numbered_folder() {
        let docs = tmp();
        std::fs::write(docs.join(FOLDER_NAME), "a file").unwrap_or_else(|e| panic!("{e}"));
        let m = make(&docs).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(
            m,
            Made {
                path: docs.join("Selene Retrieval (2)"),
                created: true
            }
        );
        assert!(docs.join(FOLDER_NAME).is_file(), "the file is untouched");
        // Second click: the numbered folder is found again, nothing new is made.
        assert!(!make(&docs).unwrap_or_else(|e| panic!("{e:?}")).created);
        assert!(!docs.join("Selene Retrieval (3)").exists());
        // Every name taken by files: a plain refusal, nothing made.
        let full = tmp();
        for n in names() {
            std::fs::write(full.join(n), "x").unwrap_or_else(|e| panic!("{e}"));
        }
        assert_eq!(make(&full), Err(MakeError::NoFreeName));
        let _ = std::fs::remove_dir_all(&docs);
        let _ = std::fs::remove_dir_all(&full);
    }

    #[cfg(windows)]
    #[test]
    fn a_junction_with_the_name_is_never_followed() {
        let docs = tmp();
        let elsewhere = tmp();
        let link = docs.join(FOLDER_NAME);
        // `mklink /J` needs no privilege (a symlink would).
        let ok = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&elsewhere)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            eprintln!("SKIP: mklink /J unavailable");
            return;
        }
        let m = make(&docs).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(m.path, docs.join("Selene Retrieval (2)"));
        assert!(m.created);
        let _ = std::fs::remove_dir(&link);
        let _ = std::fs::remove_dir_all(&docs);
        let _ = std::fs::remove_dir_all(&elsewhere);
    }

    #[test]
    fn documents_missing_or_relative() {
        let docs = tmp();
        let gone = docs.join("not-there");
        assert_eq!(make(&gone), Err(MakeError::DocumentsMissing));
        assert!(!gone.exists(), "Documents itself is never created");
        assert_eq!(
            make(Path::new("Documents")),
            Err(MakeError::DocumentsMissing)
        );
        let file = docs.join("file");
        std::fs::write(&file, "x").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(make(&file), Err(MakeError::DocumentsMissing));
        let _ = std::fs::remove_dir_all(&docs);
    }

    #[test]
    fn os_errors_map_to_plain_classes() {
        let e = |c: i32| classify(&io::Error::from_raw_os_error(c));
        assert_eq!(e(5), MakeError::AccessDenied);
        assert_eq!(e(19), MakeError::AccessDenied);
        assert_eq!(e(112), MakeError::DiskFull);
        assert_eq!(e(39), MakeError::DiskFull);
        assert_eq!(e(1295), MakeError::DiskFull);
        assert_eq!(e(3), MakeError::DocumentsMissing);
        assert_eq!(e(67), MakeError::DocumentsMissing);
        assert_eq!(e(87), MakeError::Other);
    }
}
