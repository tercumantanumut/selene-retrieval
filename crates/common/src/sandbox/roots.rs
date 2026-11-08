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
}

/// Key of a protected location: canonicalised when it exists (expands 8.3 names such as
/// `C:\Users\RUNNER~1` and resolves junctions), otherwise taken as given.
fn location_key(p: &Path) -> Vec<String> {
    match std::fs::canonicalize(p) {
        Ok(c) => key(&c),
        Err(_) => key(p),
    }
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Lower-cased, `\`-separated components without the verbatim prefix.
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
        let rules = Self {
            never_inside: inside.iter().map(|p| location_key(p)).collect(),
            never_contain: contain.iter().map(|p| location_key(p)).collect(),
            allowed_only: None,
        };
        // M5-E: the machine policy installed by the engine (unmanaged until then).
        match crate::config::policy::current()
            .allowed_folder_roots
            .clone()
        {
            Some(roots) => rules.with_allowed_roots(roots),
            None => rules,
        }
    }

    /// M5-E: roots must be inside one of `roots` (an administrator's allowed roots). Repeated
    /// calls narrow further (each root must satisfy every list).
    pub fn with_allowed_roots(mut self, roots: impl IntoIterator<Item = PathBuf>) -> Self {
        let keys: Vec<Vec<String>> = roots.into_iter().map(|p| location_key(&p)).collect();
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
                let k = key(root);
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
        // Drive root ("c:") or UNC share root ("\\server\share"): only the prefix component.
        if k.len() <= 1 {
            return true;
        }
        self.never_inside.iter().any(|p| starts_with(&k, p))
            || self.never_contain.iter().any(|p| starts_with(p, &k))
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
