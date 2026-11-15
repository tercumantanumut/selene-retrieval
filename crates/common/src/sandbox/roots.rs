//! Dangerous roots (W13; error `PathRefused` → "Pick a folder inside it instead").
//!
//! A folder is refused as a root when it
//! * is a drive root (`C:\`, `D:\`) or a UNC share root, or
//! * **contains** a protected location (e.g. the user profile contains `AppData` and `.ssh`;
//!   `C:\Users` contains every profile), or
//! * is **inside** a location that must never be indexed (Windows, Program Files, ProgramData,
//!   roaming AppData, the app's own data directory).
//!
//! Stricter than Selene's `dangerous-paths.ts`, which only blocks exact matches. Local AppData is
//! protected from being *contained* but not from being *entered*, so `%LOCALAPPDATA%\Temp\x`
//! stays usable.
//!
//! **macOS (Amendment 18)**, same three rules ([`DangerousRoots::from_env`]):
//! * Volume roots: `/`, `/Volumes` and `/Volumes/<name>` (a drive or share root), and hidden
//!   top-level system folders (`/.vol`, `/.fseventsd`, ...).
//! * Never inside: `/System`, `/Library`, `/Applications`, `/usr`, `/bin`, `/sbin`, `/opt`,
//!   `/dev`, `/cores`, `~/.config`, the app's data directory; `/private` (`/etc`, `/var`, `/tmp`)
//!   except the per-user temp folder (`confstr(_CS_DARWIN_USER_TEMP_DIR)`, the twin of
//!   `%LOCALAPPDATA%\Temp`); `~/Library` (Keychains, Mail, Cookies, app data: AppData's twin)
//!   except iCloud Drive (`~/Library/Mobile Documents`) and cloud folders
//!   (`~/Library/CloudStorage`), which hold the user's own documents (OneDrive's place on
//!   Windows is inside the profile and allowed there too).
//! * Never contained: all of the above plus `~/.ssh`, `~/.aws`, `~/.gnupg`, `~/.local`. So the
//!   home folder and `/Users` are refused, like `%USERPROFILE%` and `C:\Users`.
//!
//! The home folder comes from the account database (`getpwuid_r`), not `$HOME`. Deny lists compare
//! case-insensitively on every platform (APFS is case-insensitive by default; on a
//! case-sensitive volume this refuses more, never less). The administrator's allowed roots compare
//! exact components on Unix, so `/work` is not inside an allowed `/Work` there.

use std::path::{Component, Path, PathBuf, Prefix};

/// Policy for which folders may become roots.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DangerousRoots {
    /// Refused if the root is equal to or inside one of these (normalised keys).
    never_inside: Vec<Vec<String>>,
    /// Refused if the root is equal to or contains one of these (normalised keys).
    never_contain: Vec<Vec<String>>,
    /// M5-E: when set, a root must be equal to or inside one of these (machine policy
    /// `allowed_roots`; empty = no root allowed).
    allowed_only: Option<Vec<Vec<String>>>,
    /// Unix: refused if equal to or inside the location, unless also equal to or inside one of its
    /// exceptions (`/private` but the per-user temp folder). Empty on Windows.
    never_inside_except: Vec<(Vec<String>, Vec<Vec<String>>)>,
}

/// Key of a protected location: canonicalised when it exists (expands 8.3 names such as
/// `C:\Users\RUNNER~1` and resolves junctions), otherwise taken as given.
fn location_key(p: &Path) -> Vec<String> {
    match std::fs::canonicalize(p) {
        Ok(c) => key(&c),
        Err(_) => key(p),
    }
}

/// Key for the administrator's allowed roots: like [`location_key`], but exact components on
/// Unix (an allow-list must not widen to a differently cased folder on a case-sensitive volume).
fn allow_location_key(p: &Path) -> Vec<String> {
    match std::fs::canonicalize(p) {
        Ok(c) => allow_key(&c),
        Err(_) => allow_key(p),
    }
}

#[cfg(windows)]
fn allow_key(p: &Path) -> Vec<String> {
    key(p)
}

#[cfg(not(windows))]
fn allow_key(p: &Path) -> Vec<String> {
    p.components()
        .filter_map(|c| match c {
            Component::Normal(n) => Some(n.to_string_lossy().into_owned()),
            // Kept, so a root given with `..` that could not be canonicalised matches nothing.
            Component::ParentDir => Some("..".into()),
            _ => None,
        })
        .collect()
}

/// Drive or share root (`c:`, `\\server\share`): only the prefix component.
#[cfg(windows)]
fn is_volume_root(k: &[String]) -> bool {
    k.len() <= 1
}

/// `/`, `/Volumes`, `/Volumes/<name>` (a drive or share root), or a hidden top-level system
/// folder such as `/.vol` (files by inode number) or `/.fseventsd`.
#[cfg(not(windows))]
fn is_volume_root(k: &[String]) -> bool {
    match k.first() {
        None => true,
        Some(top) => (top == "volumes" && k.len() <= 2) || top.starts_with('.'),
    }
}

#[cfg(windows)]
fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Lower-cased, `\\`-separated components without the verbatim prefix.
fn key(p: &Path) -> Vec<String> {
    dunce::simplified(p)
        .components()
        .filter_map(|c| match c {
            // Normalise by kind so `\\?\C:` (kept verbatim beyond MAX_PATH) equals `C:`.
            Component::Prefix(px) => Some(match px.kind() {
                Prefix::Disk(d) | Prefix::VerbatimDisk(d) => {
                    format!("{}:", char::from(d).to_ascii_lowercase())
                }
                Prefix::UNC(s, h) | Prefix::VerbatimUNC(s, h) => format!(
                    r"\\{}\{}",
                    s.to_string_lossy().to_lowercase(),
                    h.to_string_lossy().to_lowercase()
                ),
                _ => px.as_os_str().to_string_lossy().to_lowercase(),
            }),
            Component::Normal(n) => Some(n.to_string_lossy().to_lowercase()),
            Component::RootDir | Component::CurDir | Component::ParentDir => None,
        })
        .collect()
}

fn starts_with(path: &[String], prefix: &[String]) -> bool {
    !prefix.is_empty() && path.len() >= prefix.len() && path[..prefix.len()] == *prefix
}

impl DangerousRoots {
    /// No protected locations (drive and share roots are still refused).
    pub fn none() -> Self {
        Self::default()
    }

    /// Protected locations from the environment of the current user. `data_dir` is the app's own
    /// data directory (never indexable).
    pub fn from_env(data_dir: Option<&Path>) -> Self {
        let rules = Self::platform_rules(data_dir);
        // M5-E: the machine policy installed by the engine (unmanaged until then).
        match crate::config::policy::current()
            .allowed_folder_roots
            .clone()
        {
            Some(roots) => rules.with_allowed_roots(roots),
            None => rules,
        }
    }

    #[cfg(windows)]
    fn platform_rules(data_dir: Option<&Path>) -> Self {
        let mut inside: Vec<PathBuf> = [
            "SystemRoot",
            "ProgramFiles",
            "ProgramFiles(x86)",
            "ProgramW6432",
            "ProgramData",
            "APPDATA",
        ]
        .iter()
        .filter_map(|v| env_path(v))
        .collect();
        if let Some(d) = data_dir {
            inside.push(d.to_path_buf());
        }
        let mut contain = inside.clone();
        if let Some(local) = env_path("LOCALAPPDATA") {
            contain.push(local);
        }
        if let Some(home) = env_path("USERPROFILE") {
            for sub in [".ssh", ".aws", ".gnupg", "AppData"] {
                contain.push(home.join(sub));
            }
        }
        Self {
            never_inside: inside.iter().map(|p| location_key(p)).collect(),
            never_contain: contain.iter().map(|p| location_key(p)).collect(),
            allowed_only: None,
            never_inside_except: Vec::new(),
        }
    }

    #[cfg(not(windows))]
    fn platform_rules(data_dir: Option<&Path>) -> Self {
        let home = home_dir();
        let mut inside: Vec<PathBuf> = UNIX_SYSTEM.iter().map(PathBuf::from).collect();
        if let Some(h) = &home {
            // Tokens of command-line tools (gh, gcloud, ...): the twin of roaming AppData.
            inside.push(h.join(".config"));
        }
        if let Some(d) = data_dir {
            inside.push(d.to_path_buf());
        }
        let mut contain = inside.clone();
        if let Some(h) = &home {
            for sub in [".ssh", ".aws", ".gnupg", ".local"] {
                contain.push(h.join(sub));
            }
        }
        #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
        let mut except: Vec<(Vec<String>, Vec<Vec<String>>)> = Vec::new();
        #[cfg(target_os = "macos")]
        {
            let temp: Vec<Vec<String>> = user_temp_dir().iter().map(|t| location_key(t)).collect();
            except.push((location_key(Path::new("/private")), temp));
            if let Some(h) = &home {
                let library = h.join("Library");
                let docs = [
                    library.join("Mobile Documents"),
                    library.join("CloudStorage"),
                ];
                except.push((
                    location_key(&library),
                    docs.iter().map(|p| location_key(p)).collect(),
                ));
            }
        }
        let mut never_contain: Vec<Vec<String>> = contain.iter().map(|p| location_key(p)).collect();
        never_contain.extend(except.iter().map(|(loc, _)| loc.clone()));
        Self {
            never_inside: inside.iter().map(|p| location_key(p)).collect(),
            never_contain,
            allowed_only: None,
            never_inside_except: except,
        }
    }

    /// M5-E: roots must be inside one of `roots` (an administrator's allowed roots). Repeated
    /// calls narrow further (each root must satisfy every list).
    pub fn with_allowed_roots(mut self, roots: impl IntoIterator<Item = PathBuf>) -> Self {
        let keys: Vec<Vec<String>> = roots.into_iter().map(|p| allow_location_key(&p)).collect();
        self.allowed_only = Some(match self.allowed_only.take() {
            None => keys,
            // The overlap of both areas: entries of either list that lie inside the other.
            Some(have) => {
                let mut out: Vec<Vec<String>> = have
                    .iter()
                    .filter(|h| keys.iter().any(|k| starts_with(h, k)))
                    .cloned()
                    .collect();
                for k in &keys {
                    if have.iter().any(|h| starts_with(k, h)) && !out.contains(k) {
                        out.push(k.clone());
                    }
                }
                out
            }
        });
        self
    }

    /// M5-E: whether `root` (ideally canonical) is inside the policy's allowed roots (always
    /// `true` when unmanaged).
    pub fn is_allowed_by_policy(&self, root: &Path) -> bool {
        match &self.allowed_only {
            None => true,
            Some(list) => {
                let k = allow_key(root);
                list.iter().any(|p| starts_with(&k, p))
            }
        }
    }

    /// Add a location that must never be a root or contain a root.
    pub fn with_never_inside(mut self, p: impl Into<PathBuf>) -> Self {
        let k = location_key(&p.into());
        self.never_contain.push(k.clone());
        self.never_inside.push(k);
        self
    }

    /// Add a location that a root must not contain (but may be inside).
    pub fn with_never_contain(mut self, p: impl Into<PathBuf>) -> Self {
        self.never_contain.push(location_key(&p.into()));
        self
    }

    /// Whether `root` (ideally canonical) is refused.
    pub fn is_refused(&self, root: &Path) -> bool {
        let k = key(root);
        if is_volume_root(&k) {
            return true;
        }
        self.never_inside.iter().any(|p| starts_with(&k, p))
            || self.never_contain.iter().any(|p| starts_with(p, &k))
            || self
                .never_inside_except
                .iter()
                .any(|(p, except)| starts_with(&k, p) && !except.iter().any(|e| starts_with(&k, e)))
    }
}

/// System folders a root may never be (or be inside) on Unix.
#[cfg(target_os = "macos")]
const UNIX_SYSTEM: &[&str] = &[
    "/System",
    "/Library",
    "/Applications",
    "/usr",
    "/bin",
    "/sbin",
    "/opt",
    "/dev",
    "/cores",
];

#[cfg(all(unix, not(target_os = "macos")))]
const UNIX_SYSTEM: &[&str] = &[
    "/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/etc", "/var", "/boot", "/dev", "/proc",
    "/sys", "/run", "/opt", "/root", "/snap",
];

/// The current account's home folder from the account database (not `$HOME`, which is the
/// caller's); `$HOME` only if the account has none.
#[cfg(unix)]
pub(super) fn home_dir() -> Option<PathBuf> {
    unix_user::home_dir().or_else(|| {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|h| h.is_absolute())
    })
}

#[cfg(target_os = "macos")]
fn user_temp_dir() -> Option<PathBuf> {
    unix_user::darwin_user_temp_dir()
}

/// libc lookups for the protected locations. Each call is one libc function with a caller-owned,
/// correctly sized buffer.
#[cfg(unix)]
#[allow(unsafe_code)] // libc FFI: getpwuid_r, confstr. Reviewed (macOS port).
mod unix_user {
    use std::ffi::{CStr, OsString};
    use std::os::unix::ffi::OsStringExt;
    use std::path::PathBuf;

    pub(super) fn home_dir() -> Option<PathBuf> {
        let mut buf = vec![0u8; 4096];
        loop {
            // SAFETY: an all-zero `passwd` is a valid out value (null pointers, zero ids).
            let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
            let mut res: *mut libc::passwd = std::ptr::null_mut();
            // SAFETY: `pw`, `buf` and `res` outlive the call and `buf.len()` is the buffer size;
            // the strings in `pw` point into `buf` and are read before `buf` changes.
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
            let dir = unsafe { CStr::from_ptr(pw.pw_dir) }.to_bytes().to_vec();
            let dir = PathBuf::from(OsString::from_vec(dir));
            return dir.is_absolute().then_some(dir);
        }
    }

    /// `confstr(_CS_DARWIN_USER_TEMP_DIR)`, e.g. `/var/folders/xx/…/T/` (not `$TMPDIR`).
    #[cfg(target_os = "macos")]
    pub(super) fn darwin_user_temp_dir() -> Option<PathBuf> {
        // SAFETY: size query: a null buffer of length 0 only returns the size needed (with NUL).
        let n = unsafe { libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, std::ptr::null_mut(), 0) };
        if n == 0 {
            return None;
        }
        let mut buf = vec![0u8; n];
        // SAFETY: `buf` holds `n` writable bytes and outlives the call.
        let m =
            unsafe { libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, buf.as_mut_ptr().cast(), n) };
        if m == 0 || m > n {
            return None;
        }
        buf.truncate(m - 1);
        let dir = PathBuf::from(OsString::from_vec(buf));
        dir.is_absolute().then_some(dir)
    }
}

#[cfg(test)]
#[cfg(windows)]
mod tests {
    use super::*;

    fn policy() -> DangerousRoots {
        DangerousRoots::none()
            .with_never_inside(r"C:\Windows")
            .with_never_inside(r"C:\Program Files")
            .with_never_inside(r"C:\Users\me\AppData\Roaming")
            .with_never_contain(r"C:\Users\me\AppData")
            .with_never_contain(r"C:\Users\me\.ssh")
    }

    #[test]
    fn refused_and_allowed_roots() {
        let p = policy();
        for r in [
            r"C:\",
            r"D:\",
            r"\\?\C:\",
            r"\\server\share",
            r"C:\Windows",
            r"c:\windows\system32",
            r"C:\Program Files\App",
            r"C:\Users",
            r"C:\Users\me",
            r"\\?\C:\Users\ME",
            r"C:\Users\me\AppData",
            r"C:\Users\me\AppData\Roaming\Claude",
        ] {
            assert!(p.is_refused(Path::new(r)), "{r} should be refused");
        }
        for r in [
            r"C:\Users\me\Documents",
            r"\\?\C:\Users\me\Documents\Projects",
            r"C:\Users\me\AppData\Local\Temp\x",
            r"D:\code",
            r"C:\Windowsish",
            r"\\server\share\team",
        ] {
            assert!(!p.is_refused(Path::new(r)), "{r} should be allowed");
        }
    }
    /// M5-E: the policy's allowed roots; repeated lists narrow to their overlap.
    #[test]
    fn allowed_roots_from_policy() {
        let p = DangerousRoots::none().with_allowed_roots([PathBuf::from(r"D:\Work")]);
        assert!(p.is_allowed_by_policy(Path::new(r"D:\Work")));
        assert!(p.is_allowed_by_policy(Path::new(r"d:\work\notes")));
        assert!(!p.is_allowed_by_policy(Path::new(r"D:\Workshop")));
        assert!(!p.is_allowed_by_policy(Path::new(r"E:\Work")));
        assert!(DangerousRoots::none().is_allowed_by_policy(Path::new(r"E:\x")));
        let none = DangerousRoots::none().with_allowed_roots(Vec::<PathBuf>::new());
        assert!(!none.is_allowed_by_policy(Path::new(r"D:\Work")));
        let narrowed = DangerousRoots::none()
            .with_allowed_roots([PathBuf::from(r"D:\")])
            .with_allowed_roots([PathBuf::from(r"D:\Work"), PathBuf::from(r"E:\Data")]);
        assert!(narrowed.is_allowed_by_policy(Path::new(r"D:\Work\a")));
        assert!(!narrowed.is_allowed_by_policy(Path::new(r"D:\Other")));
        assert!(!narrowed.is_allowed_by_policy(Path::new(r"E:\Data")));
    }
}

#[cfg(test)]
#[cfg(unix)]
mod unix_tests {
    use super::*;

    fn policy() -> DangerousRoots {
        DangerousRoots::none()
            .with_never_inside("/nonexistent-sr/System")
            .with_never_inside("/nonexistent-sr/Users/me/Library")
            .with_never_contain("/nonexistent-sr/Users/me/.ssh")
    }

    #[test]
    fn refused_and_allowed_roots() {
        let p = policy();
        for r in [
            "/",
            "/Volumes",
            "/Volumes/USB",
            "/volumes/usb",
            "/.vol",
            "/.vol/16777220/2",
            "/.fseventsd",
            "/nonexistent-sr/System",
            "/nonexistent-sr/system/Library",
            "/nonexistent-sr/Users",
            "/nonexistent-sr/Users/me",
            "/nonexistent-sr/Users/me/Library",
            "/nonexistent-sr/Users/me/Library/Keychains",
        ] {
            assert!(p.is_refused(Path::new(r)), "{r} should be refused");
        }
        for r in [
            "/nonexistent-sr/Users/me/Documents",
            "/Volumes/USB/Projects",
            "/nonexistent-sr/Systemish",
            "/nonexistent-sr/Users/me/Library2",
            "/Users/Shared/team",
        ] {
            assert!(!p.is_refused(Path::new(r)), "{r} should be allowed");
        }
    }

    #[test]
    fn exceptions_lift_only_their_own_rule() {
        let mut p = DangerousRoots::none().with_never_inside("/nonexistent-sr/data/T/app");
        p.never_inside_except.push((
            key(Path::new("/nonexistent-sr/data")),
            vec![
                key(Path::new("/nonexistent-sr/data/T")),
                key(Path::new("/nonexistent-sr/data/U")),
            ],
        ));
        assert!(p.is_refused(Path::new("/nonexistent-sr/data")));
        assert!(p.is_refused(Path::new("/nonexistent-sr/data/C/cache")));
        assert!(!p.is_refused(Path::new("/nonexistent-sr/data/U")));
        assert!(!p.is_refused(Path::new("/nonexistent-sr/data/T/x")));
        // The exception never lifts "must not contain": T holds the app data dir.
        assert!(p.is_refused(Path::new("/nonexistent-sr/data/T")));
        // The app data dir inside the excepted folder stays protected.
        assert!(p.is_refused(Path::new("/nonexistent-sr/data/T/app/sub")));
    }

    /// M5-E: allowed roots are exact on Unix; repeated lists narrow to their overlap.
    #[test]
    fn allowed_roots_from_policy() {
        let p = DangerousRoots::none().with_allowed_roots([PathBuf::from("/nonexistent-sr/Work")]);
        assert!(p.is_allowed_by_policy(Path::new("/nonexistent-sr/Work")));
        assert!(p.is_allowed_by_policy(Path::new("/nonexistent-sr/Work/notes")));
        assert!(!p.is_allowed_by_policy(Path::new("/nonexistent-sr/work/notes")));
        assert!(!p.is_allowed_by_policy(Path::new("/nonexistent-sr/Workshop")));
        assert!(DangerousRoots::none().is_allowed_by_policy(Path::new("/x")));
        let none = DangerousRoots::none().with_allowed_roots(Vec::<PathBuf>::new());
        assert!(!none.is_allowed_by_policy(Path::new("/nonexistent-sr/Work")));
        let dotdot =
            DangerousRoots::none().with_allowed_roots([PathBuf::from("/nonexistent-sr/a/../b")]);
        assert!(!dotdot.is_allowed_by_policy(Path::new("/nonexistent-sr/b")));
        assert!(!dotdot.is_allowed_by_policy(Path::new("/nonexistent-sr/a/b")));
        let narrowed = DangerousRoots::none()
            .with_allowed_roots([PathBuf::from("/nonexistent-sr/D")])
            .with_allowed_roots([
                PathBuf::from("/nonexistent-sr/D/Work"),
                PathBuf::from("/nonexistent-sr/E/Data"),
            ]);
        assert!(narrowed.is_allowed_by_policy(Path::new("/nonexistent-sr/D/Work/a")));
        assert!(!narrowed.is_allowed_by_policy(Path::new("/nonexistent-sr/D/Other")));
        assert!(!narrowed.is_allowed_by_policy(Path::new("/nonexistent-sr/E/Data")));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn macos_protected_locations() {
        let p = DangerousRoots::from_env(None);
        let home = home_dir().unwrap();
        for r in [
            "/System",
            "/System/Volumes/Data",
            "/Library",
            "/Library/Keychains",
            "/Applications",
            "/Applications/Safari.app",
            "/usr",
            "/usr/local/share",
            "/bin",
            "/sbin",
            "/opt/homebrew",
            "/dev",
            "/cores",
            "/private",
            "/private/etc",
            "/etc",
            "/var",
            "/var/db",
            "/tmp",
            "/private/var/folders",
            "/Users",
        ] {
            // Callers pass canonical paths (`/etc` is `/private/etc`).
            let c = std::fs::canonicalize(r).unwrap_or_else(|_| PathBuf::from(r));
            assert!(p.is_refused(&c), "{r} should be refused");
        }
        for sub in [
            "",
            "Library",
            "Library/Keychains",
            "Library/Application Support/Claude",
            "Library/Mail",
            "Library/Cookies",
            ".config",
            ".config/gh",
        ] {
            let r = home.join(sub);
            assert!(p.is_refused(&r), "{} should be refused", r.display());
        }
        for sub in [
            "Documents",
            "Desktop/notes",
            "Library/Mobile Documents/com~apple~CloudDocs",
            "Library/CloudStorage/OneDrive-Personal",
            "Projects/.config-backup",
        ] {
            let r = home.join(sub);
            assert!(!p.is_refused(&r), "{} should be allowed", r.display());
        }
        // The per-user temp folder (`%LOCALAPPDATA%\Temp`'s twin) stays usable.
        let temp = std::fs::canonicalize(user_temp_dir().unwrap()).unwrap();
        assert!(!p.is_refused(&temp.join("work")));
        // The data dir is protected wherever it is.
        let with_data = DangerousRoots::from_env(Some(&temp.join("sr-data")));
        assert!(with_data.is_refused(&temp.join("sr-data").join("x")));
        assert!(with_data.is_refused(&temp));
        assert!(!with_data.is_refused(&temp.join("other")));
    }
}
