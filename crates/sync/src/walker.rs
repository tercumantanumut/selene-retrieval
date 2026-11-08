//! Reconcile walker: one pass over a folder root with the `ignore` crate.
//!
//! * `.gitignore`, `.git/info/exclude`, `.ignore` and `.seleneignore` (highest precedence, so
//!   `!pattern` can re-include what `.gitignore` excludes; docs/06 change 5) are honoured even
//!   outside a git repository; the user's global gitignore and parent directories above the root
//!   are not (a folder's contents must not depend on files outside it). Matching is
//!   case-insensitive. A `.seleneignore` `!pattern` also lifts the built-in defaults
//!   ([`FolderRules::reincluded`]).
//! * [`PathFilter`] prunes directories before they are read and classifies files (Selene
//!   defaults, user globs, include set, sandbox components, secret denylist).
//! * Symlinks and junctions are not followed unless `follow_symlinks` is on, and then only when
//!   the sandbox resolves the link inside the root (`Sandbox::resolve_relative`: realpath
//!   containment, component and denylist checks on the canonical long names).
//! * **Directory completeness** (W4): every directory that the walk lists is recorded; a listing
//!   error marks that directory incomplete, an error on one entry marks its parent incomplete, and
//!   an error without a path poisons the whole pass (no deletes anywhere). The catalog sweeps
//!   unseen files only below complete directories.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use ignore::WalkBuilder;
use sr_catalog::repo::files::dir_key_of;
use sr_common::error::{Error, ErrorCode, Result};
use sr_common::sandbox::{Sandbox, Violation, path_key};
use tokio_util::sync::CancellationToken;

use crate::filter::{Excluded, PathFilter, extension_of};
use crate::fsid;
use crate::rules::{FolderRules, SELENEIGNORE};

/// One file the walk will catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkedFile {
    /// Absolute path (under the canonical root).
    pub abs: PathBuf,
    /// Root-relative, `/`-separated, long names as on disk.
    pub rel: String,
    pub path_key: String,
    pub ext: String,
    pub size: u64,
    pub mtime_ns: i64,
    /// Cloud placeholder (reading it would download it).
    pub online_only: bool,
}

/// Counters of one walk.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WalkStats {
    pub files: u64,
    pub dirs: u64,
    pub excluded_default: u64,
    pub excluded_user: u64,
    pub not_included: u64,
    pub secret: u64,
    pub rejected: u64,
    pub links_skipped: u64,
    pub errors: u64,
}

impl WalkStats {
    fn count(&mut self, e: Excluded) {
        match e {
            Excluded::Default => self.excluded_default += 1,
            Excluded::User | Excluded::Ignored => self.excluded_user += 1,
            Excluded::NotIncluded => self.not_included += 1,
            Excluded::Secret => self.secret += 1,
            Excluded::Rejected(_) => self.rejected += 1,
        }
    }
}

/// Result of a complete (not cancelled) walk.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WalkOutcome {
    /// `(rel_dir_key, complete, error)` for every listed directory (`""` = root).
    pub dirs: Vec<(String, bool, Option<ErrorCode>)>,
    pub stats: WalkStats,
}

/// Walk parameters.
#[derive(Debug, Clone)]
pub struct WalkSpec {
    /// Canonical root (from `Sandbox::add_root`).
    pub root: PathBuf,
    /// The sandbox whose root 0 is `root` (used for followed links).
    pub sandbox: Arc<Sandbox>,
    pub filter: Arc<PathFilter>,
    /// The folder's ignore files (re-includes of built-in defaults).
    pub rules: Arc<FolderRules>,
    pub recursive: bool,
    pub follow_symlinks: bool,
    /// Files per callback batch.
    pub batch: usize,
}

/// Root-relative `/` path of `p`, or `None` if `p` is not under `root` or not valid Unicode.
pub fn rel_of(root: &Path, p: &Path) -> Option<String> {
    let rest = p.strip_prefix(root).ok()?;
    let mut out = Vec::new();
    for c in rest.components() {
        out.push(c.as_os_str().to_str()?.to_string());
    }
    Some(out.join("/"))
}

fn mtime_ns(md: &std::fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .and_then(|d| i64::try_from(d.as_nanos()).ok())
        .unwrap_or(0)
}

fn err_path(e: &ignore::Error) -> Option<&Path> {
    match e {
        ignore::Error::WithPath { path, .. } => Some(path),
        ignore::Error::WithDepth { err, .. } | ignore::Error::WithLineNumber { err, .. } => {
            err_path(err)
        }
        ignore::Error::Loop { child, .. } => Some(child),
        ignore::Error::Partial(errs) => errs.first().and_then(err_path),
        _ => None,
    }
}

fn err_code(e: &ignore::Error) -> ErrorCode {
    match e.io_error() {
        Some(io) if io.kind() == std::io::ErrorKind::PermissionDenied => {
            ErrorCode::PermissionDenied
        }
        Some(io) if matches!(io.raw_os_error(), Some(32 | 33)) => ErrorCode::FileLocked,
        Some(io) if io.kind() == std::io::ErrorKind::NotFound => ErrorCode::NotFound,
        _ => ErrorCode::Internal,
    }
}

/// A followed link is kept only if the sandbox resolves it inside the root.
fn link_contained(sandbox: &Sandbox, rel: &str) -> bool {
    sandbox.resolve_relative(0, rel).is_ok()
}

/// Walks `spec.root`, calling `on_batch` with up to `spec.batch` files at a time. Returns
/// `Err(Cancelled)` if `cancel` fires (the caller must then not sweep).
pub fn walk(
    spec: &WalkSpec,
    cancel: &CancellationToken,
    mut on_batch: impl FnMut(Vec<WalkedFile>) -> Result<()>,
) -> Result<WalkOutcome> {
    let root = spec.root.clone();
    let filter = Arc::clone(&spec.filter);
    let rules = Arc::clone(&spec.rules);
    let sandbox = Arc::clone(&spec.sandbox);
    let follow = spec.follow_symlinks;
    let mut b = WalkBuilder::new(&spec.root);
    b.standard_filters(false)
        .hidden(false)
        .parents(false)
        .ignore(true)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(false)
        .require_git(false)
        .add_custom_ignore_filename(SELENEIGNORE)
        .ignore_case_insensitive(true)
        .follow_links(follow)
        .same_file_system(false)
        .max_depth(if spec.recursive { None } else { Some(1) })
        .sort_by_file_name(std::cmp::Ord::cmp)
        .filter_entry(move |e| {
            if e.depth() == 0 {
                return true;
            }
            let Some(rel) = rel_of(&root, e.path()) else {
                return false;
            };
            let is_dir = e.file_type().is_some_and(|t| t.is_dir());
            if e.path_is_symlink() && (!follow || !link_contained(&sandbox, &rel)) {
                // Not followed (or escapes the root): never descended, never cataloged.
                return !is_dir && !follow;
            }
            !is_dir || filter.check_dir_with(&rel, Some(&rules)).is_ok()
        });

    let mut dirs: HashMap<String, (bool, Option<ErrorCode>)> = HashMap::new();
    let mut poisoned = false;
    let mut stats = WalkStats::default();
    let mut batch: Vec<WalkedFile> = Vec::with_capacity(spec.batch);
    let mark_incomplete =
        |dirs: &mut HashMap<String, (bool, Option<ErrorCode>)>, key: &str, code: ErrorCode| {
            let e = dirs.entry(key.to_string()).or_insert((false, None));
            e.0 = false;
            e.1.get_or_insert(code);
        };

    for item in b.build() {
        if cancel.is_cancelled() {
            return Err(Error::new(ErrorCode::Cancelled));
        }
        let entry = match item {
            Ok(e) => e,
            Err(e) => {
                stats.errors += 1;
                let code = err_code(&e);
                match err_path(&e).and_then(|p| rel_of(&spec.root, p)) {
                    Some(rel) => {
                        let key = path_key(&rel);
                        if dirs.contains_key(&key) {
                            mark_incomplete(&mut dirs, &key, code);
                        } else {
                            mark_incomplete(&mut dirs, dir_key_of(&key), code);
                        }
                    }
                    None => poisoned = true,
                }
                tracing::debug!(error = %e, "walk error");
                continue;
            }
        };
        let Some(rel) = rel_of(&spec.root, entry.path()) else {
            stats.rejected += 1;
            continue;
        };
        let ft = entry.file_type();
        if entry.depth() == 0 {
            dirs.entry(String::new()).or_insert((true, None));
            continue;
        }
        if ft.is_some_and(|t| t.is_dir()) {
            stats.dirs += 1;
            if spec.recursive {
                dirs.entry(path_key(&rel)).or_insert((true, None));
            }
            continue;
        }
        if !ft.is_some_and(|t| t.is_file()) || (entry.path_is_symlink() && !follow) {
            stats.links_skipped += 1;
            continue;
        }
        if let Err(x) = spec.filter.check_file_with(&rel, Some(&spec.rules)) {
            stats.count(x);
            continue;
        }
        let md = match entry.metadata() {
            Ok(m) => m,
            Err(e) => {
                // The file exists but can't be stat'ed: keep whatever the catalog has for it.
                stats.errors += 1;
                mark_incomplete(&mut dirs, dir_key_of(&path_key(&rel)), err_code(&e));
                continue;
            }
        };
        let name = rel.rsplit('/').next().unwrap_or(&rel);
        batch.push(WalkedFile {
            abs: entry.path().to_path_buf(),
            path_key: path_key(&rel),
            ext: extension_of(name),
            size: md.len(),
            mtime_ns: mtime_ns(&md),
            online_only: fsid::is_online_only(&md),
            rel,
        });
        stats.files += 1;
        if batch.len() >= spec.batch.max(1) {
            on_batch(std::mem::take(&mut batch))?;
        }
    }
    if !batch.is_empty() {
        on_batch(batch)?;
    }
    if cancel.is_cancelled() {
        return Err(Error::new(ErrorCode::Cancelled));
    }
    let mut out: Vec<(String, bool, Option<ErrorCode>)> = dirs
        .into_iter()
        .map(|(k, (done, code))| (k, done && !poisoned, code))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(WalkOutcome { dirs: out, stats })
}

/// Validates a watcher-supplied relative path through the sandbox: realpath containment (expands
/// 8.3 names, resolves junctions and symlinks), component rules and the denylist on the canonical
/// long names. Returns the canonical absolute path and the long root-relative path.
pub fn resolve_event_path(
    sandbox: &Sandbox,
    rel: &str,
) -> std::result::Result<(PathBuf, String), Violation> {
    match sandbox.resolve_relative(0, rel) {
        Ok(r) => Ok((r.path, r.rel)),
        Err(e) => Err(e.violation().unwrap_or(Violation::OutsideRoot)),
    }
}
