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
//!
//! **macOS / Unix.** A GUI app does not get the shell's `PATH`, so `PATH` is never read; the home
//! folder comes from the user database (`getpwuid_r`), never `$HOME`. Order: Claude
//! `~/.local/bin/claude` (native installer), `~/.claude/local/claude` (older local install), then
//! the package-manager folders; Codex the package-manager folders, then `~/.local/bin/codex`. The
//! package-manager folders are the npm `prefix` set in `~/.npmrc`, `~/.npm-global/bin`,
//! `/opt/homebrew/bin`, `/usr/local/bin`, `~/.volta/bin`, `~/.bun/bin`, `~/Library/pnpm` and the
//! nvm versions (newest first). The file must be a regular file with an execute bit on a local
//! volume, reached through at most 16 links; the file and every folder above it and above each
//! link (both as written and resolved) must be owned by this user or root and not writable by
//! others (group-writable only for `wheel` / `admin`, as Homebrew's folders are). Anything else
//! could be replaced by another account and is skipped. The npm shims start `node` through
//! `/usr/bin/env`; [`super::process`] puts the CLI's own folder first on the child's `PATH`.

#[cfg(windows)]
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
#[cfg(windows)]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchEnv {
    pub userprofile: Option<OsString>,
    pub appdata: Option<OsString>,
    pub path: Option<OsString>,
}

#[cfg(windows)]
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
#[cfg(windows)]
fn is_local_absolute(p: &Path) -> bool {
    let s = p.as_os_str().to_string_lossy();
    p.is_absolute() && !s.starts_with(r"\\") && !s.starts_with("//")
}

/// Only `.exe` and `.cmd`, by exact (case-insensitive) extension.
#[cfg(windows)]
fn allowed_ext(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("exe") || e.eq_ignore_ascii_case("cmd"))
}

/// Candidate paths in search order (they may not exist).
#[cfg(windows)]
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
#[cfg(windows)]
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
#[cfg(windows)]
pub fn accept(path: &Path) -> Option<CliPath> {
    (is_local_absolute(path) && allowed_ext(path) && path.is_file()).then(|| CliPath {
        path: path.to_path_buf(),
    })
}

// ------------------------------------------------------------------------------- macOS / Unix

/// The environment the search reads: only the home folder (from the user database).
#[cfg(unix)]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchEnv {
    pub home: Option<PathBuf>,
}

#[cfg(unix)]
impl SearchEnv {
    pub fn from_process() -> Self {
        Self {
            home: crate::sys::home_dir(),
        }
    }
}

/// Most symbolic links followed from a candidate to the file.
#[cfg(unix)]
const MAX_LINKS: usize = 16;

/// Groups that may have write access to a CLI's folders: `wheel`, and on macOS `admin` (Homebrew
/// makes its folders group-writable for admins, who can change any system file anyway).
#[cfg(target_os = "macos")]
const TRUSTED_GROUPS: &[u32] = &[0, 80];
#[cfg(all(unix, not(target_os = "macos")))]
const TRUSTED_GROUPS: &[u32] = &[0];

/// Absolute and without `..`.
#[cfg(unix)]
fn is_clean_absolute(p: &Path) -> bool {
    p.is_absolute()
        && !p
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
}

/// The npm `prefix` from `~/.npmrc` (absolute or `~/...`), if set.
#[cfg(unix)]
fn npmrc_prefix(home: &Path) -> Option<PathBuf> {
    use std::io::Read;
    let mut text = String::new();
    std::fs::File::open(home.join(".npmrc"))
        .ok()?
        .take(64 * 1024)
        .read_to_string(&mut text)
        .ok()?;
    let value = text.lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        (k.trim() == "prefix").then(|| v.trim().trim_matches(['"', '\'']).to_owned())
    })?;
    let p = match value.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(value),
    };
    is_clean_absolute(&p).then_some(p)
}

/// `~/.nvm/versions/node/v*/bin`, newest version first.
#[cfg(unix)]
fn nvm_bins(home: &Path) -> Vec<PathBuf> {
    let root = home.join(".nvm").join("versions").join("node");
    let Ok(rd) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut found: Vec<(Vec<u64>, PathBuf)> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let v: Option<Vec<u64>> = name
                .strip_prefix('v')?
                .split('.')
                .map(|n| n.parse().ok())
                .collect();
            Some((v?, root.join(&name).join("bin")))
        })
        .collect();
    found.sort_by(|a, b| b.0.cmp(&a.0));
    found.into_iter().map(|(_, p)| p).collect()
}

/// Folders a package manager puts a global `claude` / `codex` into, in search order.
#[cfg(unix)]
fn package_dirs(home: Option<&Path>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(h) = home {
        out.extend(npmrc_prefix(h).map(|p| p.join("bin")));
        out.push(h.join(".npm-global").join("bin"));
    }
    out.push(PathBuf::from("/opt/homebrew/bin"));
    out.push(PathBuf::from("/usr/local/bin"));
    if let Some(h) = home {
        out.push(h.join(".volta").join("bin"));
        out.push(h.join(".bun").join("bin"));
        out.push(h.join("Library").join("pnpm"));
        out.extend(nvm_bins(h));
    }
    out
}

/// Candidate paths in search order (they may not exist).
#[cfg(unix)]
pub fn candidates(kind: CliKind, env: &SearchEnv) -> Vec<PathBuf> {
    let stem = kind.stem();
    let home = env.home.as_deref().filter(|h| is_clean_absolute(h));
    let local_bin = home.map(|h| h.join(".local").join("bin"));
    let mut dirs: Vec<PathBuf> = Vec::new();
    match kind {
        CliKind::Claude => {
            dirs.extend(local_bin);
            dirs.extend(home.map(|h| h.join(".claude").join("local")));
            dirs.extend(package_dirs(home));
        }
        CliKind::Codex => {
            dirs.extend(package_dirs(home));
            dirs.extend(local_bin);
        }
    }
    let mut out: Vec<PathBuf> = Vec::new();
    for p in dirs.into_iter().map(|d| d.join(stem)) {
        if is_clean_absolute(&p) && !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// Owned by this user or root, not writable by others, group-writable only for a trusted group.
#[cfg(unix)]
fn owner_ok(md: &std::fs::Metadata, me: u32) -> bool {
    use std::os::unix::fs::MetadataExt;
    let mode = md.mode();
    (md.uid() == me || md.uid() == 0)
        && mode & 0o002 == 0
        && (mode & 0o020 == 0 || TRUSTED_GROUPS.contains(&md.gid()))
}

/// `dir` and every folder above it, both as written and resolved, pass [`owner_ok`].
#[cfg(unix)]
fn dir_chain_trusted(dir: &Path, me: u32) -> bool {
    let Ok(real) = std::fs::canonicalize(dir) else {
        return false;
    };
    [dir, real.as_path()].iter().all(|d| {
        d.ancestors().all(|a| {
            a.as_os_str().is_empty()
                || std::fs::metadata(a).is_ok_and(|m| m.is_dir() && owner_ok(&m, me))
        })
    })
}

/// The Unix trust rules (module docs): every link hop and the final file.
#[cfg(unix)]
fn trusted(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let me = crate::sys::uid();
    let mut cur = path.to_path_buf();
    for _ in 0..=MAX_LINKS {
        let Some(dir) = cur.parent() else {
            return false;
        };
        if !dir_chain_trusted(dir, me) {
            return false;
        }
        let Ok(md) = std::fs::symlink_metadata(&cur) else {
            return false;
        };
        if md.file_type().is_symlink() {
            let Ok(target) = std::fs::read_link(&cur) else {
                return false;
            };
            // An absolute target replaces `dir`; a relative one is taken from the link's folder.
            cur = dir.join(target);
            continue;
        }
        return md.is_file()
            && owner_ok(&md, me)
            && md.permissions().mode() & 0o111 != 0
            && crate::sys::on_local_volume(&cur);
    }
    false
}

/// The first candidate that passes the trust rules.
#[cfg(unix)]
pub fn find_with(kind: CliKind, env: &SearchEnv) -> Option<CliPath> {
    candidates(kind, env)
        .into_iter()
        .find(|p| trusted(p))
        .map(|path| CliPath { path })
}

/// Accepts a path given by a caller (tests, an Advanced override): same rules as the search.
#[cfg(unix)]
pub fn accept(path: &Path) -> Option<CliPath> {
    (is_clean_absolute(path) && trusted(path)).then(|| CliPath {
        path: path.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    fn env(home: &Path, path: &str) -> SearchEnv {
        SearchEnv {
            userprofile: Some(home.as_os_str().to_owned()),
            appdata: Some(home.join("AppData").join("Roaming").into_os_string()),
            path: Some(path.into()),
        }
    }

    #[cfg(windows)]
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

    #[cfg(windows)]
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

    #[cfg(unix)]
    #[test]
    fn search_order_and_rules() {
        let home = PathBuf::from("/Users/ana-no-such-user");
        let e = SearchEnv {
            home: Some(home.clone()),
        };
        let c = candidates(CliKind::Claude, &e);
        assert_eq!(c[0], home.join(".local/bin/claude"));
        assert_eq!(c[1], home.join(".claude/local/claude"));
        assert_eq!(c[2], home.join(".npm-global/bin/claude"));
        assert_eq!(c[3], PathBuf::from("/opt/homebrew/bin/claude"));
        assert_eq!(c[4], PathBuf::from("/usr/local/bin/claude"));
        assert!(c.iter().all(|p| is_clean_absolute(p)), "{c:?}");
        let x = candidates(CliKind::Codex, &e);
        assert_eq!(x[0], home.join(".npm-global/bin/codex"));
        assert_eq!(x.last(), Some(&home.join(".local/bin/codex")));
        // No home: only the system-wide folders, never a bare name or a relative path.
        let none = candidates(CliKind::Claude, &SearchEnv::default());
        assert_eq!(
            none,
            vec![
                PathBuf::from("/opt/homebrew/bin/claude"),
                PathBuf::from("/usr/local/bin/claude")
            ]
        );
        let rel = SearchEnv {
            home: Some(PathBuf::from("Users/ana")),
        };
        assert_eq!(candidates(CliKind::Claude, &rel), none);
    }

    #[cfg(unix)]
    #[test]
    fn npm_prefix_and_nvm_versions() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::write(home.join(".npmrc"), "; c\nprefix = ~/npm-root\n").unwrap();
        assert_eq!(npmrc_prefix(home), Some(home.join("npm-root")));
        std::fs::write(home.join(".npmrc"), "prefix=../x\n").unwrap();
        assert_eq!(npmrc_prefix(home), None, "relative prefix");
        for v in ["v9.11.2", "v22.1.0", "v20.18.3", "system"] {
            std::fs::create_dir_all(home.join(".nvm/versions/node").join(v)).unwrap();
        }
        let nvm = home.join(".nvm/versions/node");
        assert_eq!(
            nvm_bins(home),
            vec![
                nvm.join("v22.1.0/bin"),
                nvm.join("v20.18.3/bin"),
                nvm.join("v9.11.2/bin")
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn finds_only_trusted_executables() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let set_mode = |p: &Path, m: u32| {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();
        };
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        if !dir_chain_trusted(&home, crate::sys::uid()) {
            eprintln!(
                "temp folder {} is shared; trust checks not testable here",
                home.display()
            );
            return;
        }
        let bin = home.join(".local/bin");
        let real = home.join(".local/share/claude/versions");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&real).unwrap();
        let exe = real.join("2.1.0");
        std::fs::write(&exe, "#!/bin/sh\n").unwrap();
        // The native installer's shape: ~/.local/bin/claude -> ../share/claude/versions/2.1.0.
        symlink("../share/claude/versions/2.1.0", bin.join("claude")).unwrap();
        let e = SearchEnv {
            home: Some(home.clone()),
        };
        set_mode(&exe, 0o644);
        assert_eq!(find_with(CliKind::Claude, &e), None, "no execute bit");
        set_mode(&exe, 0o755);
        assert_eq!(
            find_with(CliKind::Claude, &e).map(|p| p.path),
            Some(bin.join("claude"))
        );
        assert!(accept(&exe).is_some());
        // Writable by others: the file, or a folder on the way (as written or resolved).
        set_mode(&exe, 0o757);
        assert_eq!(accept(&exe), None, "file writable by others");
        set_mode(&exe, 0o755);
        set_mode(&real, 0o777);
        assert_eq!(
            accept(&bin.join("claude")),
            None,
            "target folder writable by others"
        );
        set_mode(&real, 0o755);
        set_mode(&bin, 0o777);
        assert_eq!(
            accept(&bin.join("claude")),
            None,
            "link folder writable by others"
        );
        set_mode(&bin, 0o755);
        // Not a regular file, relative, `..`, missing, a link loop.
        assert_eq!(accept(&real), None, "folder");
        assert_eq!(accept(Path::new("claude")), None, "relative");
        assert_eq!(
            accept(&bin.join("../share/claude/versions/2.1.0")),
            None,
            ".."
        );
        assert_eq!(accept(&bin.join("codex")), None, "missing");
        symlink("loop", bin.join("loop")).unwrap();
        assert_eq!(accept(&bin.join("loop")), None, "link loop");
        assert!(accept(&bin.join("claude")).is_some());
    }
}
