//! `SandboxFs`: every file the engine opens on behalf of a folder or a client goes through here
//! (docs/02 §2.6 "Path traversal").
//!
//! 1. **Policy** ([`sr_common::sandbox::Sandbox`]): syntax rules (no `..`, ADS, device names,
//!    trailing dot/space, UNC/verbatim prefixes), realpath-of-the-first-existing-ancestor
//!    containment (8.3 names expanded, junctions and symlinks resolved, like Selene's
//!    `path-utils.ts`), and the always-on [`SecretFilter`](sr_common::sandbox::SecretFilter).
//! 2. **Open** relative to a cap-std `Dir` held for each root, so a directory swapped for a junction
//!    between the check and the open still cannot escape the root (residual TOCTOU of step 1).
//!
//! The extract worker never sees a path: ingest hands it the `std::fs::File` returned here.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use cap_std::ambient_authority;
use cap_std::fs::Dir;
use sr_common::error::{Error, ErrorCode};
use sr_common::ids::FolderId;
use sr_common::sandbox::{DangerousRoots, ResolvedPath, Sandbox, SandboxError, Violation};

/// One allowed root.
#[derive(Debug)]
struct Root {
    folder_id: FolderId,
    /// As stored in the catalog (display form).
    root_path: PathBuf,
    dir: Dir,
}

/// A resolved file inside one folder.
#[derive(Debug, Clone)]
pub struct Located {
    pub folder_id: FolderId,
    /// The folder root as stored in the catalog.
    pub root_path: PathBuf,
    pub resolved: ResolvedPath,
}

impl Located {
    /// Root-relative path, `/`-separated.
    pub fn rel(&self) -> &str {
        &self.resolved.rel
    }
    /// Absolute display path (`C:\…`).
    pub fn display_path(&self) -> PathBuf {
        self.resolved.display_path()
    }
}

/// Policy + capability handles for the current folder set. Rebuilt when folders change.
#[derive(Debug)]
pub struct SandboxFs {
    sandbox: Sandbox,
    roots: Vec<Root>,
}

/// Map a sandbox refusal to the one error model, keeping only the file name in context.
pub fn sandbox_error(e: SandboxError, input: &str) -> Error {
    let err = Error::from(e);
    let name = Path::new(input)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned());
    match name {
        Some(n) => err.with_name(n),
        None => err,
    }
}

impl SandboxFs {
    /// Build from `(folder, root)` pairs. Roots that are missing or refused are returned
    /// separately (the folder becomes `unavailable`; it is never deleted).
    pub fn build(roots: &[(FolderId, PathBuf)], data_dir: &Path) -> (Self, Vec<(FolderId, Error)>) {
        let mut sandbox = Sandbox::with_policy(DangerousRoots::from_env(Some(data_dir)));
        let mut ok = Vec::new();
        let mut failed = Vec::new();
        for (folder_id, root_path) in roots {
            // M5-E (R6): a network share that doesn't answer within seconds is unavailable for
            // now (its index and files are kept), so one hung share never blocks the start or
            // a folder change.
            if !sr_sync::netprobe::reachable(root_path) {
                failed.push((
                    *folder_id,
                    Error::new(ErrorCode::FolderUnavailable)
                        .with_detail("network folder didn't answer in time"),
                ));
                continue;
            }
            // Trial on a copy, so a root whose handle cannot be opened leaves policy and handles
            // aligned (root index i in the sandbox == ok[i]).
            let mut trial = sandbox.clone();
            match trial.add_root(root_path) {
                Ok(i) => {
                    let canonical = trial.roots()[i].canonical.clone();
                    match Dir::open_ambient_dir(&canonical, ambient_authority()) {
                        Ok(dir) => {
                            sandbox = trial;
                            ok.push(Root {
                                folder_id: *folder_id,
                                root_path: root_path.clone(),
                                dir,
                            });
                        }
                        Err(e) => failed.push((*folder_id, Error::from(e))),
                    }
                }
                Err(e) => failed.push((*folder_id, Error::from(e))),
            }
        }
        (Self { sandbox, roots: ok }, failed)
    }

    /// No roots (start-up before folders load, tests).
    pub fn empty(data_dir: &Path) -> Self {
        Self::build(&[], data_dir).0
    }

    pub fn folder_ids(&self) -> Vec<FolderId> {
        self.roots.iter().map(|r| r.folder_id).collect()
    }

    fn index_of(&self, folder_id: FolderId) -> Option<usize> {
        self.roots.iter().position(|r| r.folder_id == folder_id)
    }

    fn located(&self, resolved: ResolvedPath) -> Result<Located, Error> {
        let r = self
            .roots
            .get(resolved.root)
            .ok_or_else(|| Error::new(ErrorCode::PathOutsideSandbox))?;
        Ok(Located {
            folder_id: r.folder_id,
            root_path: r.root_path.clone(),
            resolved,
        })
    }

    /// A root-relative path inside one folder (ingest).
    pub fn resolve_in(&self, folder_id: FolderId, rel: &str) -> Result<Located, Error> {
        let i = self
            .index_of(folder_id)
            .ok_or_else(|| Error::new(ErrorCode::FolderUnavailable))?;
        let r = self
            .sandbox
            .resolve_relative(i, rel)
            .map_err(|e| sandbox_error(e, rel))?;
        self.located(r)
    }

    /// A client-supplied path: absolute (inside any folder), or relative to a folder. A relative
    /// path is tried against each folder in order and the first one where it exists wins; if it
    /// exists nowhere the first folder's resolution is returned (so the caller reports NotFound).
    /// Optionally restricted to `scope` folders.
    pub fn resolve_client(&self, path: &str, scope: Option<&[FolderId]>) -> Result<Located, Error> {
        if path.trim().is_empty() {
            return Err(Error::new(ErrorCode::InvalidInput).with_detail("empty path"));
        }
        if self.roots.is_empty() {
            return Err(Error::new(ErrorCode::PathOutsideSandbox).with_detail("no folders"));
        }
        let in_scope = |l: &Located| scope.is_none_or(|s| s.contains(&l.folder_id));
        let b = path.as_bytes();
        let absolute = path.starts_with(['/', '\\'])
            || (b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic());
        if absolute {
            let r = self
                .sandbox
                .resolve(path)
                .map_err(|e| sandbox_error(e, path))?;
            let l = self.located(r)?;
            return if in_scope(&l) {
                Ok(l)
            } else {
                Err(Error::new(ErrorCode::PathOutsideSandbox)
                    .with_detail("outside requested scope"))
            };
        }
        let mut first: Option<Result<Located, Error>> = None;
        for i in 0..self.roots.len() {
            let res = self
                .sandbox
                .resolve_relative(i, path)
                .map_err(|e| sandbox_error(e, path))
                .and_then(|r| self.located(r));
            match res {
                Ok(l) if !in_scope(&l) => continue,
                Ok(l) if l.resolved.path.exists() => return Ok(l),
                // Policy refusals (denylist, traversal …) are the same for every root.
                Err(e) if e.code != ErrorCode::NotFound => return Err(e),
                other => {
                    if first.is_none() {
                        first = Some(other);
                    }
                }
            }
        }
        first.unwrap_or_else(|| {
            Err(Error::new(ErrorCode::PathOutsideSandbox).with_detail("outside requested scope"))
        })
    }

    /// Open a located regular file through the root's capability handle.
    pub fn open(&self, l: &Located) -> Result<std::fs::File, Error> {
        let i = self
            .index_of(l.folder_id)
            .ok_or_else(|| Error::new(ErrorCode::FolderUnavailable))?;
        let root = &self.roots[i];
        let name = || {
            Path::new(l.rel())
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        };
        if l.rel().is_empty() {
            return Err(Error::new(ErrorCode::InvalidInput).with_detail("path is a folder"));
        }
        let f = root
            .dir
            .open(l.rel())
            .map_err(|e| map_open_error(e).with_name(name()))?;
        let meta = f.metadata().map_err(|e| Error::from(e).with_name(name()))?;
        if !meta.is_file() {
            return Err(Error::new(ErrorCode::NotFound)
                .with_name(name())
                .with_detail("not a regular file"));
        }
        Ok(f.into_std())
    }

    /// Open a directory below a root (grep walks).
    pub fn root_dir(&self, folder_id: FolderId) -> Option<(&Dir, &Path)> {
        self.index_of(folder_id)
            .map(|i| (&self.roots[i].dir, self.roots[i].root_path.as_path()))
    }

    /// Canonical root path of a folder (walks).
    pub fn canonical_root(&self, folder_id: FolderId) -> Option<PathBuf> {
        self.index_of(folder_id)
            .map(|i| self.sandbox.roots()[i].canonical.clone())
    }

    /// The secret denylist (always on).
    pub fn is_denied_rel(&self, rel: &str) -> bool {
        self.sandbox.secret_filter().is_denied_rel(rel)
    }
}

/// cap-std refuses escapes with `PermissionDenied`-like errors; report them as outside-the-sandbox.
fn map_open_error(e: std::io::Error) -> Error {
    let msg = e.to_string();
    if msg.contains("outside of the filesystem sandbox") || msg.contains("escape") {
        return Error::new(ErrorCode::PathOutsideSandbox)
            .with_detail(format!("{:?}", Violation::OutsideRoot));
    }
    Error::from(e)
}

/// Shared, swappable handle (rebuilt when folders change).
pub type SharedSandbox = Arc<std::sync::RwLock<Arc<SandboxFs>>>;
