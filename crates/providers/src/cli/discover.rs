//! Finding the CLI by **absolute path** (M5-C).
//!
//! Order:
//! 1. the CLI's own install folders: Claude `%USERPROFILE%\.local\bin\claude.exe` (native
//!    installer), then the npm shim `%APPDATA%\npm\claude.cmd`; Codex `%APPDATA%\npm\codex.cmd`
//!    (npm), then `%USERPROFILE%\.local\bin\codex.exe`;
//! 2. each `PATH` entry that is an absolute folder on a local drive, `<stem>.exe` then
//!    `<stem>.cmd`.
//!
//! Never the current directory (Windows' own search would try it first), never a relative or
//! network (`\\server\...`) path, never another extension (`.bat`, `.ps1`, `.js`). The file must
//! exist. A `.cmd` shim is launched by Rust's standard library, which runs it through
//! `%SystemRoot%\System32\cmd.exe` and refuses arguments it can't quote safely for it.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::CliKind;

/// A CLI found on this computer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliPath {
    /// Absolute path of `claude.exe` / `codex.cmd` / ….
    pub path: PathBuf,
}

impl CliPath {
    /// Whether this is a `.cmd` shim (npm), run through `cmd.exe`.
    pub fn is_shim(&self) -> bool {
        self.path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("cmd"))
    }
}

/// The environment the search reads (the daemon runs as the signed-in user).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchEnv {
    pub userprofile: Option<OsString>,
    pub appdata: Option<OsString>,
    pub path: Option<OsString>,
}

impl SearchEnv {
    pub fn from_process() -> Self {
        Self {
            userprofile: std::env::var_os("USERPROFILE"),
            appdata: std::env::var_os("APPDATA"),
            path: std::env::var_os("PATH"),
        }
    }
}

/// An absolute path on a local drive (`C:\...`), not a share, not relative.
fn is_local_absolute(p: &Path) -> bool {
    let s = p.as_os_str().to_string_lossy();
    p.is_absolute() && !s.starts_with(r"\\") && !s.starts_with("//")
}

/// Only `.exe` and `.cmd`, by exact (case-insensitive) extension.
fn allowed_ext(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("exe") || e.eq_ignore_ascii_case("cmd"))
}

/// Candidate paths in search order (they may not exist).
pub fn candidates(kind: CliKind, env: &SearchEnv) -> Vec<PathBuf> {
    let stem = kind.stem();
    let home = env.userprofile.as_ref().map(PathBuf::from);
    let appdata = env.appdata.as_ref().map(PathBuf::from);
    let local_bin = home.map(|h| h.join(".local").join("bin").join(format!("{stem}.exe")));
    let npm = appdata.map(|a| a.join("npm").join(format!("{stem}.cmd")));
    let mut out: Vec<PathBuf> = match kind {
        CliKind::Claude => [local_bin, npm].into_iter().flatten().collect(),
        CliKind::Codex => [npm, local_bin].into_iter().flatten().collect(),
    };
    if let Some(path) = &env.path {
        for dir in std::env::split_paths(path) {
            // `.`, `bin`, `` and other relative entries would resolve against the current
            // directory: skipped.
            if !is_local_absolute(&dir) {
                continue;
            }
            out.push(dir.join(format!("{stem}.exe")));
            out.push(dir.join(format!("{stem}.cmd")));
        }
    }
    out.retain(|p| is_local_absolute(p) && allowed_ext(p));
    out
}

/// The first existing candidate.
pub fn find_with(kind: CliKind, env: &SearchEnv) -> Option<CliPath> {
    candidates(kind, env)
        .into_iter()
        .find(|p| p.is_file())
        .map(|path| CliPath { path })
}

/// [`find_with`] over this process's environment.
pub fn find(kind: CliKind) -> Option<CliPath> {
    find_with(kind, &SearchEnv::from_process())
}

/// Accepts a path given by a caller (tests, an Advanced override): same rules as the search.
pub fn accept(path: &Path) -> Option<CliPath> {
    (is_local_absolute(path) && allowed_ext(path) && path.is_file()).then(|| CliPath {
        path: path.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(home: &Path, path: &str) -> SearchEnv {
        SearchEnv {
            userprofile: Some(home.as_os_str().to_owned()),
            appdata: Some(home.join("AppData").join("Roaming").into_os_string()),
            path: Some(path.into()),
        }
    }

    #[test]
    fn search_order_and_rules() {
        let home = PathBuf::from(r"C:\Users\Ana");
        let c = candidates(
            CliKind::Claude,
            &env(&home, r".;bin;C:\Tools;\\server\share;;C:\Node"),
        );
        assert_eq!(c[0], home.join(r".local\bin\claude.exe"));
        assert_eq!(c[1], home.join(r"AppData\Roaming\npm\claude.cmd"));
        assert_eq!(c[2], PathBuf::from(r"C:\Tools\claude.exe"));
        assert_eq!(c[3], PathBuf::from(r"C:\Tools\claude.cmd"));
        assert_eq!(c[4], PathBuf::from(r"C:\Node\claude.exe"));
        assert_eq!(
            c.len(),
            6,
            "no current-directory, relative or network entries: {c:?}"
        );
        let x = candidates(CliKind::Codex, &env(&home, ""));
        assert_eq!(x[0], home.join(r"AppData\Roaming\npm\codex.cmd"));
        assert_eq!(x[1], home.join(r".local\bin\codex.exe"));
        // No environment at all: nothing, never a bare name.
        assert!(candidates(CliKind::Claude, &SearchEnv::default()).is_empty());
    }

    #[test]
    fn finds_only_existing_absolute_exe_or_cmd() {
        let dir = tempfile::tempdir().unwrap();
        let tools = dir.path().join("tools");
        std::fs::create_dir_all(&tools).unwrap();
        // A decoy in the "current directory" style entry and a .bat are never picked.
        std::fs::write(tools.join("codex.bat"), "@echo off").unwrap();
        let e = SearchEnv {
            userprofile: None,
            appdata: None,
            path: Some(tools.clone().into_os_string()),
        };
        assert_eq!(find_with(CliKind::Codex, &e), None);
        std::fs::write(tools.join("codex.cmd"), "@echo off").unwrap();
        assert_eq!(
            find_with(CliKind::Codex, &e).map(|p| p.path),
            Some(tools.join("codex.cmd"))
        );
        assert!(find_with(CliKind::Codex, &e).is_some_and(|p| p.is_shim()));
        assert_eq!(accept(Path::new("codex.cmd")), None, "relative");
        assert_eq!(accept(&tools.join("codex.bat")), None, "extension");
        assert!(accept(&tools.join("codex.cmd")).is_some());
    }
}
