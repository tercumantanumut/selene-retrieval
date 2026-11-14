//! Names shared by every client, the launcher and the daemon: data dir, pipe name, lock files,
//! scheduled-task name. All of them derive from (user SID, data dir), so tests, relocated data
//! dirs and side-by-side builds never collide, and two users on one machine (RDP + console)
//! never share a daemon.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use sr_common::error::{Error, ErrorCode};

use crate::sys;

/// Folder under `%LOCALAPPDATA%` (known-folder API, never the environment); on macOS under
/// `~/Library/Application Support` (home from the user database).
pub const DATA_DIR_NAME: &str = "SeleneRetrieval";
/// Held by the daemon for its whole life (single host per user + data dir).
pub const HOST_LOCK: &str = "host.lock";
/// Held by the one client that is launching the daemon (closes the spawn race).
pub const SPAWN_LOCK: &str = "spawn.lock";
/// Launcher log (one line per decision; read by tests and "Copy diagnostics").
pub const LAUNCH_LOG: &str = "launch.log";
/// Longest data-dir path we accept (Amendment 3.12: LanceDB fails on long paths).
pub const MAX_DATA_DIR_CHARS: usize = 200;

/// Test/dev hooks. Honoured **only in debug builds**: a release build always uses the compiled-in
/// locations (U3: env vars never silently override the product). Names:
/// `SR_DATA_DIR`, `SR_LAUNCH_EXE`, `SR_DAEMON_EXE`, `SR_TASK_PREFIX`, `SR_LAUNCH_ORDER`,
/// `SR_PROTO_OVERRIDE`.
pub fn dev_env(name: &str) -> Option<OsString> {
    if cfg!(debug_assertions) {
        std::env::var_os(name).filter(|v| !v.is_empty())
    } else {
        None
    }
}

/// `%LOCALAPPDATA%\SeleneRetrieval` (macOS: `~/Library/Application Support/SeleneRetrieval`).
pub fn default_data_dir() -> Result<PathBuf, Error> {
    sys::local_appdata()
        .map(|p| p.join(DATA_DIR_NAME))
        .map_err(|e| Error::new(ErrorCode::Internal).with_source(e))
}

/// Explicit dir (`--data-dir`), else the dev hook, else the default. Validated per 3.12.
pub fn resolve_data_dir(explicit: Option<&Path>) -> Result<PathBuf, Error> {
    let dir = match explicit {
        Some(p) => p.to_path_buf(),
        None => match dev_env("SR_DATA_DIR") {
            Some(p) => PathBuf::from(p),
            None => default_data_dir()?,
        },
    };
    validate_data_dir(&dir)?;
    Ok(dir)
}

/// A short, plain, absolute local path (no UNC, no `\\?\`, no `..`).
pub fn validate_data_dir(dir: &Path) -> Result<(), Error> {
    let s = dir.to_string_lossy();
    let bad = !dir.is_absolute()
        || s.starts_with(r"\\")
        || s.starts_with("//")
        || s.chars().count() > MAX_DATA_DIR_CHARS
        || dir
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir));
    if bad {
        return Err(Error::new(ErrorCode::SettingsInvalid)
            .with_section(sr_common::error::Section::System)
            .with_detail("data directory must be a short local absolute path"));
    }
    Ok(())
}

/// Case- and separator-insensitive key of a data dir (what the pipe/task names hash).
#[cfg(windows)]
pub fn data_dir_key(dir: &Path) -> String {
    let s = dir.to_string_lossy().replace('/', "\\");
    s.trim_end_matches('\\').to_lowercase()
}

/// Unix key of a data dir (what the socket name hashes): the path's components joined with `/`,
/// byte for byte, so `//`, `/./` and a trailing `/` don't matter but case does (R52: a
/// case-sensitive volume may hold both `SR` and `sr`, and they must never share a socket). No
/// file system access, so a client and a daemon that start before and after the folder exists
/// agree. Another spelling of the same folder gives another key; the data dir's host lock then
/// keeps a second daemon out, so a client never reaches a daemon of another folder.
#[cfg(not(windows))]
pub fn data_dir_key(dir: &Path) -> String {
    use std::path::Component;
    let mut out = String::new();
    for c in dir.components() {
        match c {
            Component::RootDir => {}
            Component::CurDir => continue,
            other => {
                out.push('/');
                out.push_str(&other.as_os_str().to_string_lossy());
            }
        }
    }
    if out.is_empty() && dir.has_root() {
        out.push('/');
    }
    out
}

fn hex(bytes: &[u8], n: usize) -> String {
    let h = blake3::hash(bytes).to_hex();
    h.as_str()[..n].to_string()
}

/// `\\.\pipe\sr-<hex(blake3(SID))[..16]>-<hex(blake3(data dir key))[..16]>`.
/// The SID is hashed so the name doesn't reveal it; the DACL (user SID only) is the real guard.
#[cfg(windows)]
pub fn pipe_name(user_sid: &str, data_dir: &Path) -> String {
    format!(
        r"\\.\pipe\sr-{}-{}",
        hex(user_sid.as_bytes(), 16),
        hex(data_dir_key(data_dir).as_bytes(), 16)
    )
}

/// Unix: the socket's file name `sr-<hex(blake3(uid))[..16]>-<hex(blake3(data dir key))[..16]>.sock`
/// (same inputs as the pipe name; `user_sid` is the decimal uid). The folder comes from
/// `sys::socket_dir` (private, per user), so the full path fits `sun_path` and never holds the
/// data dir itself.
#[cfg(unix)]
pub fn pipe_name(user_sid: &str, data_dir: &Path) -> String {
    format!(
        "sr-{}-{}.sock",
        hex(user_sid.as_bytes(), 16),
        hex(data_dir_key(data_dir).as_bytes(), 16)
    )
}

/// File name of one of our programs on this platform: `sr-daemon.exe` on Windows, `sr-daemon`
/// elsewhere.
pub fn exe_file_name(stem: &str) -> String {
    format!("{stem}{}", std::env::consts::EXE_SUFFIX)
}

/// Per-user on-demand scheduled task: `\SR\daemon-<sid8>` for the default data dir,
/// `\SR\daemon-<sid8>-<dir8>` otherwise. The SID part keeps two users' tasks apart (the task
/// store is machine-wide). In debug builds `SR_TASK_PREFIX=<p>` gives `\<p>-<sid8>-<dir8>` instead
/// (tests use `SR-M1C-test`), so parallel tests never share a task.
pub fn task_name(user_sid: &str, data_dir: &Path) -> String {
    let sid = hex(user_sid.as_bytes(), 8);
    if let Some(p) = dev_env("SR_TASK_PREFIX") {
        return format!(
            r"\{}-{sid}-{}",
            p.to_string_lossy(),
            hex(data_dir_key(data_dir).as_bytes(), 8)
        );
    }
    let is_default = default_data_dir()
        .map(|d| data_dir_key(&d) == data_dir_key(data_dir))
        .unwrap_or(false);
    if is_default {
        format!(r"\SR\daemon-{sid}")
    } else {
        format!(
            r"\SR\daemon-{sid}-{}",
            hex(data_dir_key(data_dir).as_bytes(), 8)
        )
    }
}

/// Sibling executable of the current one (`sr-launch.exe`, `sr-daemon.exe`).
pub fn sibling_exe(name: &str) -> Result<PathBuf, Error> {
    let me = std::env::current_exe().map_err(|e| Error::new(ErrorCode::Internal).with_source(e))?;
    Ok(me.with_file_name(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn pipe_name_is_stable_and_hides_the_sid() {
        let sid = "S-1-5-21-1111111111-2222222222-3333333333-1001";
        let d = Path::new(r"C:\Users\u\AppData\Local\SeleneRetrieval");
        let a = pipe_name(sid, d);
        assert_eq!(a, pipe_name(sid, d));
        assert_eq!(
            a,
            pipe_name(sid, Path::new(r"c:/users/U/AppData/Local/SeleneRetrieval/"))
        );
        assert!(a.starts_with(r"\\.\pipe\sr-"));
        assert_eq!(a.len(), r"\\.\pipe\sr-".len() + 16 + 1 + 16);
        assert!(!a.contains("1001"));
        assert_ne!(a, pipe_name("S-1-5-21-1-2-3-1002", d));
        assert_ne!(a, pipe_name(sid, Path::new(r"D:\other")));
    }

    #[cfg(unix)]
    #[test]
    fn socket_name_is_short_stable_and_hides_the_uid() {
        let d = Path::new("/Users/u/Library/Application Support/SeleneRetrieval");
        let a = pipe_name("501", d);
        assert_eq!(a, pipe_name("501", d));
        assert!(a.starts_with("sr-") && a.ends_with(".sock"), "{a}");
        assert_eq!(a.len(), "sr-".len() + 16 + 1 + 16 + ".sock".len());
        assert!(!a.contains('/') && !a.contains("501") && !a.contains("Library"));
        assert_ne!(a, pipe_name("502", d));
        assert_ne!(a, pipe_name("501", Path::new("/Users/u/other")));
        assert_eq!(exe_file_name("sr-daemon"), "sr-daemon");
        // R52: case is kept (a case-sensitive volume may hold both); redundant separators not.
        let sr = Path::new("/Volumes/data/SR");
        assert_ne!(
            pipe_name("501", sr),
            pipe_name("501", Path::new("/Volumes/data/sr"))
        );
        for same in [
            "/Volumes/data/SR/",
            "/Volumes//data/./SR",
            "/Volumes/data/SR//",
        ] {
            assert_eq!(
                pipe_name("501", sr),
                pipe_name("501", Path::new(same)),
                "{same}"
            );
        }
        assert_eq!(data_dir_key(sr), "/Volumes/data/SR");
        assert_eq!(data_dir_key(Path::new("/")), "/");
        // A backslash is an ordinary file-name byte on Unix.
        assert_ne!(
            data_dir_key(Path::new("/a\\b")),
            data_dir_key(Path::new("/a/b"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn data_dir_rules() {
        assert!(validate_data_dir(Path::new(r"C:\x\y")).is_ok());
        assert!(validate_data_dir(Path::new(r"relative")).is_err());
        assert!(validate_data_dir(Path::new(r"\\server\share\x")).is_err());
        assert!(validate_data_dir(Path::new(r"\\?\C:\x")).is_err());
        assert!(validate_data_dir(Path::new(r"C:\x\..\y")).is_err());
        let long = format!(r"C:\{}", "a".repeat(MAX_DATA_DIR_CHARS));
        assert!(validate_data_dir(Path::new(&long)).is_err());
        assert!(default_data_dir().unwrap().ends_with(DATA_DIR_NAME));
    }

    #[test]
    fn task_names_are_per_user_and_per_dir() {
        if dev_env("SR_TASK_PREFIX").is_some() {
            return;
        }
        let sid = "S-1-5-21-1-2-3-1001";
        let def = default_data_dir().unwrap();
        let t = task_name(sid, &def);
        assert!(
            t.starts_with(r"\SR\daemon-") && t.len() == r"\SR\daemon-".len() + 8,
            "{t}"
        );
        let other = task_name(sid, Path::new(r"D:\sr"));
        assert!(other.starts_with(&t) && other.len() == t.len() + 9);
        assert_ne!(t, task_name("S-1-5-21-1-2-3-1002", &def));
    }
}
