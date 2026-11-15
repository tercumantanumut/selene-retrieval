//! Move the search index to another local drive (M4-E, page 32, OQ-49).
//!
//! What moves: [`crate::datadir::INDEX_ENTRIES`] (`catalog.db*`, `lance\`, `tantivy\`, `models\`).
//! What stays in the anchor (`%LOCALAPPDATA%\SeleneRetrieval`): settings, logs, locks and the
//! pointer file, so the pipe name, the scheduled task and every MCP client config stay valid.
//!
//! Flow (crash-safe at every step):
//! 1. `index_location.move` checks the target ([`check`]), writes the plan
//!    (`index-move.json`, state `pending`) and asks the daemon to drain and exit.
//! 2. The next daemon start runs [`run_pending`] **before** the engine opens any store: protected
//!    ACL on the new folder first (4.14), then copy (`copying`), verify (size + BLAKE3 of every
//!    file), and one atomic pointer write (temp + rename, state `switched`).
//! 3. The engine starts on the new location; only then [`cleanup_after_start`] deletes the old
//!    copy (index entries only, never settings or logs).
//!
//! A crash before the pointer write leaves the old index in use (the copy resumes next time,
//! skipping files already verified). A crash after it leaves the new index in use (cleanup runs
//! after the next successful start). A failed or cancelled copy deletes only what it created.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sr_common::api::{INDEX_DIR_NAME, IndexMoveCheck, IndexMoveProgress, MovePhase, MoveProblem};
use sr_common::error::{Error, ErrorCode};
use sr_common::events::EngineEvent;

use crate::datadir::{self, INDEX_ENTRIES};

/// The plan file in the anchor.
pub const PLAN_FILE: &str = "index-move.json";
/// Head room on top of the index size: size × 1.1 + 512 MiB.
pub const SPACE_MARGIN: u64 = 512 * 1024 * 1024;
/// Copy buffer.
const CHUNK: usize = 1024 * 1024;
/// Progress events at most this often (4 Hz).
const PROGRESS_EVERY: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanState {
    /// Saved by `index_location.move`; runs at the next daemon start.
    Pending,
    /// The copy started (the new folder exists and is ours).
    Copying,
    /// The pointer names the new folder; the old copy is deleted after a successful start.
    Switched,
}

/// `index-move.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MovePlan {
    pub from: PathBuf,
    pub to: PathBuf,
    pub state: PlanState,
    pub created_at: i64,
}

pub fn read_plan(anchor: &Path) -> Option<MovePlan> {
    let text = std::fs::read_to_string(anchor.join(PLAN_FILE)).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn write_plan(anchor: &Path, plan: &MovePlan) -> Result<(), Error> {
    let tmp = anchor.join(format!("{PLAN_FILE}.tmp"));
    let body = serde_json::to_vec_pretty(plan)
        .map_err(|e| Error::new(ErrorCode::Internal).with_source(e))?;
    datadir::write_synced(&tmp, &body)?;
    std::fs::rename(&tmp, anchor.join(PLAN_FILE))
        .map_err(|e| Error::from(e).with_detail("saving the index move plan"))
}

pub fn remove_plan(anchor: &Path) {
    let _ = std::fs::remove_file(anchor.join(PLAN_FILE));
}

/// Progress and cancellation shared by the daemon (which runs the move before the engine starts)
/// and the app-RPC front end.
#[derive(Debug, Default)]
pub struct MoveControl {
    cancel: AtomicBool,
    progress: Mutex<Option<IndexMoveProgress>>,
}

impl MoveControl {
    pub fn progress(&self) -> Option<IndexMoveProgress> {
        self.progress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn set(&self, p: Option<IndexMoveProgress>) {
        *self
            .progress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = p;
    }

    /// A saved move that will run now: "waiting" until the copy starts (cancel works from here).
    pub fn set_waiting(&self, plan: &MovePlan) {
        if plan.state != PlanState::Switched {
            self.set(Some(IndexMoveProgress {
                phase: MovePhase::Waiting,
                done_bytes: 0,
                total_bytes: 0,
                from: plan.from.display().to_string(),
                to: plan.to.display().to_string(),
            }));
        }
    }

    /// Ask a running copy to stop. Only possible before the switch.
    pub fn cancel(&self) -> bool {
        let cancellable = self.progress().is_some_and(|p| {
            matches!(
                p.phase,
                MovePhase::Waiting | MovePhase::Copying | MovePhase::Verifying
            )
        });
        if cancellable {
            self.cancel.store(true, Ordering::SeqCst);
        }
        cancellable
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }
}

fn key(p: &Path) -> String {
    sr_client::naming::data_dir_key(p)
}

/// `a` is `b` or inside it (path keys: case-insensitive on Windows, exact on Unix).
fn inside(a: &Path, b: &Path) -> bool {
    let (a, b) = (key(a), key(b));
    let sep = std::path::MAIN_SEPARATOR;
    a == b || a.starts_with(&format!("{}{sep}", b.trim_end_matches(sep)))
}

fn overlaps(a: &Path, b: &Path) -> bool {
    inside(a, b) || inside(b, a)
}

/// Unix: `p` with symlinks resolved in its longest existing part (the rest appended as given),
/// so `/var/…` and `/private/var/…`, or a link into an added folder, compare as the same place.
/// Folder roots are stored resolved. Windows: unchanged (roots are compared by path key).
#[cfg(unix)]
fn real(p: &Path) -> PathBuf {
    let mut base = p.to_path_buf();
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(r) = std::fs::canonicalize(&base) {
            let mut out = r;
            for c in rest.iter().rev() {
                out.push(c);
            }
            return out;
        }
        match (base.file_name().map(|n| n.to_os_string()), base.parent()) {
            (Some(n), Some(parent)) => {
                rest.push(n);
                base = parent.to_path_buf();
            }
            _ => return p.to_path_buf(),
        }
    }
}

#[cfg(windows)]
fn real(p: &Path) -> PathBuf {
    p.to_path_buf()
}

/// `X:` of a drive-letter path.
#[cfg(windows)]
fn drive_of(p: &Path) -> Option<String> {
    let s = p.to_string_lossy();
    let mut c = s.chars();
    match (c.next(), c.next()) {
        (Some(l), Some(':')) if l.is_ascii_alphabetic() => {
            Some(format!("{}:", l.to_ascii_uppercase()))
        }
        _ => None,
    }
}

/// Unix: the mount point of the volume holding `p` (`/`, `/Volumes/Data`).
#[cfg(unix)]
fn drive_of(p: &Path) -> Option<String> {
    crate::win::volume(p).map(|v| v.mount_point.display().to_string())
}

/// Unix: the target is on a volume of this Mac that keeps file owners: `MNT_LOCAL`, not
/// removable media, not a network file system (also when a driver reports it local), and not
/// mounted with "Ignore ownership" (there every account owns every file). Fails closed when the
/// volume can't be read.
#[cfg(unix)]
fn local_volume(p: &Path) -> bool {
    const NETWORK: &[&str] = &["smbfs", "nfs", "afpfs", "webdav", "cifs", "ftp"];
    crate::win::volume(p).is_some_and(|v| {
        v.local && !v.removable && !v.ignores_owners && !NETWORK.contains(&v.fs_type.as_str())
    })
}

/// R52 (Unix): the copy goes by path, so no folder on the way to `p` may be one another
/// account can rename things in: every existing folder from `p` up to `/` is ours or root's,
/// and one that its group or others can write to must have the sticky bit (like `/tmp`, where
/// only an entry's owner may rename it). Links on the way are judged by what they point to.
#[cfg(unix)]
fn path_is_private(p: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let me = sr_client::sys::uid();
    let mut cur = Some(p);
    while let Some(dir) = cur {
        match std::fs::metadata(dir) {
            Ok(m) => {
                let owner_ok = m.uid() == me || m.uid() == 0;
                let shared = m.mode() & 0o022 != 0;
                let sticky = m.mode() & 0o1000 != 0;
                if !m.is_dir() || !owner_ok || (shared && !sticky) {
                    return false;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return false,
        }
        cur = dir.parent();
    }
    true
}

/// Index files (relative path, size) and directories (relative paths).
type IndexListing = (Vec<(PathBuf, u64)>, Vec<PathBuf>);

/// Every file of the index under `root` (relative path, size), and its directories.
fn index_files(root: &Path) -> std::io::Result<IndexListing> {
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    for e in INDEX_ENTRIES {
        let p = root.join(e);
        let Ok(md) = std::fs::symlink_metadata(&p) else {
            continue;
        };
        if md.is_file() {
            files.push((PathBuf::from(e), md.len()));
        } else if md.is_dir() {
            walk(root, &p, &mut files, &mut dirs)?;
        }
    }
    Ok((files, dirs))
}

fn walk(
    root: &Path,
    dir: &Path,
    files: &mut Vec<(PathBuf, u64)>,
    dirs: &mut Vec<PathBuf>,
) -> std::io::Result<()> {
    if let Ok(rel) = dir.strip_prefix(root) {
        dirs.push(rel.to_path_buf());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let md = entry.metadata()?;
        let p = entry.path();
        // Reparse points are never followed (nothing of ours is one).
        if md.file_type().is_symlink() {
            continue;
        }
        if md.is_dir() {
            walk(root, &p, files, dirs)?;
        } else if md.is_file()
            && let Ok(rel) = p.strip_prefix(root)
        {
            files.push((rel.to_path_buf(), md.len()));
        }
    }
    Ok(())
}

/// Size of the index under `root` (0 if unreadable).
pub fn index_size(root: &Path) -> u64 {
    index_files(root).map_or(0, |(f, _)| f.iter().map(|(_, n)| n).sum())
}

/// Bytes needed on the target for an index of `size` bytes.
pub fn needed_bytes(size: u64) -> u64 {
    size.saturating_add(size / 10).saturating_add(SPACE_MARGIN)
}

/// Where the index goes for a picked folder: `<target>\SeleneRetrieval Index`, or the anchor itself
/// when the user picks the default location again.
pub fn index_path_for(anchor: &Path, target: &Path) -> PathBuf {
    if key(target) == key(anchor) {
        anchor.to_path_buf()
    } else {
        target.join(INDEX_DIR_NAME)
    }
}

/// Inputs of [`check`].
#[derive(Debug, Clone)]
pub struct CheckInput<'a> {
    pub anchor: &'a Path,
    /// The current index root.
    pub current: &'a Path,
    /// The folder the user picked.
    pub target: &'a str,
    /// Every added folder's root.
    pub folder_roots: &'a [PathBuf],
    /// The install directory.
    pub app_dir: &'a Path,
    /// Size of the current index (bytes).
    pub size: u64,
}

/// Validate a target (page 32 rules). `InvalidInput` only for an empty or relative path; every
/// other problem is a plain [`MoveProblem`] the UI words itself.
pub fn check(i: &CheckInput<'_>) -> Result<IndexMoveCheck, Error> {
    let raw = i.target.trim().trim_end_matches(['\\', '/']);
    if raw.is_empty() || !Path::new(raw).is_absolute() {
        return Err(
            Error::new(ErrorCode::InvalidInput).with_detail("absolute folder path required")
        );
    }
    let target = PathBuf::from(raw);
    let index_path = index_path_for(i.anchor, &target);
    let drive = drive_of(&target).unwrap_or_default();
    let need = needed_bytes(i.size);
    let free = sr_providers::sys::free_disk(&target).unwrap_or(0);
    let problem = problem_for(i, &target, &index_path, &drive);
    Ok(IndexMoveCheck {
        ok: problem.is_none(),
        problem,
        index_path: index_path.display().to_string(),
        drive,
        need_bytes: need,
        free_bytes: free,
        enough_space: free >= need,
    })
}

fn problem_for(
    i: &CheckInput<'_>,
    target: &Path,
    index_path: &Path,
    drive: &str,
) -> Option<MoveProblem> {
    #[cfg(windows)]
    {
        let s = target.to_string_lossy();
        if drive.is_empty()
            || s.starts_with(r"\\")
            || sr_sync::fsid::is_unc(target)
            || sr_sync::fsid::drive_kind(target) != sr_sync::fsid::DriveKind::Local
        {
            return Some(MoveProblem::NotLocal);
        }
    }
    #[cfg(unix)]
    if drive.is_empty() || !local_volume(target) {
        return Some(MoveProblem::NotLocal);
    }
    if datadir::validate_index_root(index_path).is_err() {
        return Some(MoveProblem::TooLong);
    }
    let index_path = &real(index_path);
    let (current, anchor, app_dir) = (&real(i.current), &real(i.anchor), &real(i.app_dir));
    if key(index_path) == key(current) {
        return Some(MoveProblem::SameAsCurrent);
    }
    let back_home = key(index_path) == key(anchor);
    if overlaps(index_path, app_dir)
        || overlaps(index_path, current)
        || (!back_home && overlaps(index_path, anchor))
    {
        return Some(MoveProblem::InsideApp);
    }
    if i.folder_roots
        .iter()
        .any(|r| overlaps(index_path, &real(r)))
    {
        return Some(MoveProblem::InsideFolder);
    }
    let not_empty = if back_home {
        INDEX_ENTRIES.iter().any(|e| index_path.join(e).exists())
    } else {
        std::fs::read_dir(index_path).is_ok_and(|mut d| d.next().is_some())
    };
    if not_empty {
        return Some(MoveProblem::NotEmpty);
    }
    // S-02 (R39): an existing empty folder made by another account can't become ours.
    if !back_home && index_path.exists() && !owned_by_us(index_path) {
        return Some(MoveProblem::NotWritable);
    }
    if !target.is_dir() || !writable(target) {
        return Some(MoveProblem::NotWritable);
    }
    // R52: another account must not be able to swap a folder on the way to the copy.
    #[cfg(unix)]
    if !path_is_private(index_path) {
        return Some(MoveProblem::NotWritable);
    }
    None
}

/// Owner of `p` is us (or Administrators) and `p` is no link (S-02).
#[cfg(windows)]
fn owned_by_us(p: &Path) -> bool {
    if p.is_symlink() {
        return false;
    }
    let (Ok(me), Ok(sddl)) = (
        sr_client::win::user_sid_string(),
        sr_client::win::path_security_sddl(p),
    ) else {
        return false;
    };
    datadir::owner_of(&sddl).is_some_and(|o| {
        let o = sr_client::win::canonical_sid(o).unwrap_or_else(|_| o.to_string());
        datadir::owner_is_acceptable(&o, &me)
    })
}

/// Unix: `p` is no link and its owner is this uid (S-02).
#[cfg(unix)]
fn owned_by_us(p: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(p)
        .is_ok_and(|m| !m.file_type().is_symlink() && m.uid() == sr_client::sys::uid())
}

fn writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".sr-write-test-{}", sr_common::ids::TraceId::new()));
    let ok = std::fs::write(&probe, b"x").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

/// What [`run_pending`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoveOutcome {
    /// No plan.
    Nothing,
    /// The pointer now names `to` (the old copy is deleted after a successful start).
    Switched { from: PathBuf, to: PathBuf },
    /// Nothing changed; the index stays at `kept`.
    Failed { code: ErrorCode, kept: PathBuf },
}

/// Remove what a failed/cancelled copy created: our own folder at `to`, or (moving back to the
/// default location) only the index entries in the anchor.
fn discard_copy(anchor: &Path, plan: &MovePlan) {
    if key(&plan.to) == key(anchor) {
        delete_entries(&plan.to);
    } else if plan
        .to
        .file_name()
        .is_some_and(|n| n.to_string_lossy() == INDEX_DIR_NAME)
    {
        let _ = std::fs::remove_dir_all(&plan.to);
    }
}

/// Delete the index entries of an old location (never anything else).
fn delete_entries(root: &Path) {
    for e in INDEX_ENTRIES {
        let p = root.join(e);
        let r = if p.is_dir() {
            std::fs::remove_dir_all(&p)
        } else {
            std::fs::remove_file(&p)
        };
        if let Err(err) = r
            && err.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(target: "sr_engine::indexmove", entry = e, err = %err, "old index entry not deleted");
        }
    }
}

/// Run a saved move (blocking; the daemon calls it before the engine starts). Publishes
/// `index.move.progress` (≤ 4 Hz), then `index.move.done` or `index.move.failed`.
pub fn run_pending(
    anchor: &Path,
    ctl: &MoveControl,
    publish: &(dyn Fn(EngineEvent) + Send + Sync),
) -> MoveOutcome {
    let Some(mut plan) = read_plan(anchor) else {
        return MoveOutcome::Nothing;
    };
    if plan.state == PlanState::Switched {
        // The copy finished and the pointer moved; cleanup happens after the start.
        return MoveOutcome::Switched {
            from: plan.from,
            to: plan.to,
        };
    }
    // S-02: only a target folder we created or claimed is ever cleaned up. A pre-existing folder
    // refused before the claim (not empty, another account's) is left exactly as it was.
    let mut claimed = false;
    let fail = |plan: &MovePlan, e: &Error, ctl: &MoveControl, claimed: bool| {
        tracing::warn!(target: "sr_engine::indexmove", code = %e.code, err = %e, "index move failed; nothing changed");
        if claimed {
            discard_copy(anchor, plan);
        }
        remove_plan(anchor);
        ctl.set(None);
        publish(EngineEvent::IndexMoveFailed {
            code: e.code,
            kept_path: plan.from.display().to_string(),
        });
        MoveOutcome::Failed {
            code: e.code,
            kept: plan.from.clone(),
        }
    };
    match copy_and_switch(anchor, &mut plan, ctl, publish, &mut claimed) {
        Ok(()) => {
            ctl.set(None);
            publish(EngineEvent::IndexMoveDone {
                path: plan.to.display().to_string(),
            });
            MoveOutcome::Switched {
                from: plan.from,
                to: plan.to,
            }
        }
        Err(e) => fail(&plan, &e, ctl, claimed),
    }
}

fn copy_and_switch(
    anchor: &Path,
    plan: &mut MovePlan,
    ctl: &MoveControl,
    publish: &(dyn Fn(EngineEvent) + Send + Sync),
    claimed: &mut bool,
) -> Result<(), Error> {
    let (from, to) = (plan.from.clone(), plan.to.clone());
    datadir::validate_index_root(&to)?;
    if key(&from) == key(&to) {
        return Err(Error::new(ErrorCode::InvalidInput).with_detail("same location"));
    }
    // The pointer must name the plan's source, or the plan is stale (moved some other way).
    let current = datadir::read_pointer(anchor).unwrap_or_else(|| anchor.to_path_buf());
    if key(&current) != key(&from) {
        return Err(Error::new(ErrorCode::Conflict)
            .with_detail("the index is no longer where the plan started"));
    }
    let (files, dirs) =
        index_files(&from).map_err(|e| Error::from(e).with_detail("reading the current index"))?;
    let total: u64 = files.iter().map(|(_, n)| n).sum();
    let mut prog = IndexMoveProgress {
        phase: MovePhase::Copying,
        done_bytes: 0,
        total_bytes: total,
        from: from.display().to_string(),
        to: to.display().to_string(),
    };
    ctl.set(Some(prog.clone()));
    publish(EngineEvent::IndexMoveProgress(prog.clone()));
    // Space (again: the drive may have filled up since the check).
    let free = sr_providers::sys::free_disk(&to).unwrap_or(u64::MAX);
    if free < needed_bytes(total) {
        let mut e = Error::new(ErrorCode::DiskFull);
        e.ctx.need = Some(format!("{} MB", needed_bytes(total).div_ceil(1_000_000)));
        e.ctx.drive = drive_of(&to);
        return Err(e);
    }
    // 1. Our folder with the protected ACL before any file lands in it (4.14).
    if key(&to) == key(anchor) {
        std::fs::create_dir_all(&to)
            .map_err(|e| Error::from(e).with_detail("creating the new index folder"))?;
        *claimed = true;
    } else {
        // R52 (Unix): checked again now, before anything is created (the move runs at the next
        // start, so the folders on the way may have changed since the check).
        #[cfg(unix)]
        if !path_is_private(&to) {
            return Err(Error::new(ErrorCode::PermissionDenied)
                .with_name(to.display().to_string())
                .with_detail("another account can change a folder on the way to the new index"));
        }
        // S-02 (R39): create the leaf ourselves. A folder that already exists (someone made it
        // before the copy, or a resumed copy) must be empty on a fresh start, and in every case
        // owned by us; then its owner + DACL are reset in one call (foreign ACEs dropped).
        match std::fs::create_dir(&to) {
            Ok(()) => *claimed = true,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if plan.state == PlanState::Pending
                    && std::fs::read_dir(&to).is_ok_and(|mut d| d.next().is_some())
                {
                    return Err(Error::new(ErrorCode::Conflict)
                        .with_name(to.display().to_string())
                        .with_detail("the new index folder is not empty"));
                }
            }
            Err(e) => {
                return Err(Error::from(e).with_detail("creating the new index folder"));
            }
        }
        datadir::protect_index_dir(&to)?;
        // Ours now (created by us, or an empty folder of ours whose ACL was just reset), or a
        // resumed copy of ours: a later failure may clean it up.
        *claimed = true;
    }
    if plan.state == PlanState::Pending {
        plan.state = PlanState::Copying;
        write_plan(anchor, plan)?;
    }
    for d in &dirs {
        std::fs::create_dir_all(to.join(d))
            .map_err(|e| Error::from(e).with_detail("creating index folders"))?;
    }
    // 2. Copy (resumable: a file already there with the same size and hash is kept).
    let mut hashes: HashMap<PathBuf, blake3::Hash> = HashMap::new();
    let mut last = Instant::now();
    for (rel, size) in &files {
        let src = from.join(rel);
        let dst = to.join(rel);
        let src_hash = hash_file(&src, ctl)?;
        let same = std::fs::metadata(&dst).is_ok_and(|m| m.len() == *size)
            && hash_file(&dst, ctl).is_ok_and(|h| h == src_hash);
        if !same {
            copy_file(&src, &dst, ctl, &mut |n| {
                prog.done_bytes = prog.done_bytes.saturating_add(n);
                if last.elapsed() >= PROGRESS_EVERY {
                    last = Instant::now();
                    ctl.set(Some(prog.clone()));
                    publish(EngineEvent::IndexMoveProgress(prog.clone()));
                }
            })?;
        } else {
            prog.done_bytes = prog.done_bytes.saturating_add(*size);
        }
        hashes.insert(rel.clone(), src_hash);
    }
    // 3. Verify every file against the source hash.
    prog.phase = MovePhase::Verifying;
    prog.done_bytes = 0;
    ctl.set(Some(prog.clone()));
    publish(EngineEvent::IndexMoveProgress(prog.clone()));
    for (rel, size) in &files {
        let dst = to.join(rel);
        let ok = std::fs::metadata(&dst).is_ok_and(|m| m.len() == *size)
            && hash_file(&dst, ctl)? == hashes[rel];
        if !ok {
            return Err(Error::new(ErrorCode::Internal)
                .with_detail(format!("copy of {} did not verify", rel.display())));
        }
        prog.done_bytes = prog.done_bytes.saturating_add(*size);
        if last.elapsed() >= PROGRESS_EVERY {
            last = Instant::now();
            ctl.set(Some(prog.clone()));
            publish(EngineEvent::IndexMoveProgress(prog.clone()));
        }
    }
    if ctl.cancelled() {
        return Err(Error::new(ErrorCode::Cancelled));
    }
    // 4. The switch: one atomic pointer write. From here on there is no cancel.
    prog.phase = MovePhase::Switching;
    ctl.set(Some(prog.clone()));
    publish(EngineEvent::IndexMoveProgress(prog.clone()));
    if key(&to) == key(anchor) {
        datadir::write_pointer(anchor, None)?;
    } else {
        datadir::write_pointer(anchor, Some(&to))?;
    }
    plan.state = PlanState::Switched;
    if let Err(e) = write_plan(anchor, plan) {
        // The pointer already moved; the old copy just stays until a later cleanup.
        tracing::warn!(target: "sr_engine::indexmove", err = %e, "move plan not updated after the switch");
    }
    tracing::info!(target: "sr_engine::indexmove", bytes = total, files = files.len(), "search index moved");
    Ok(())
}

fn hash_file(path: &Path, ctl: &MoveControl) -> Result<blake3::Hash, Error> {
    let mut f = std::fs::File::open(path)
        .map_err(|e| Error::from(e).with_detail(format!("opening {}", path.display())))?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        if ctl.cancelled() {
            return Err(Error::new(ErrorCode::Cancelled));
        }
        let n = f
            .read(&mut buf)
            .map_err(|e| Error::from(e).with_detail("reading the index"))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize())
}

fn copy_file(
    src: &Path,
    dst: &Path,
    ctl: &MoveControl,
    on_bytes: &mut dyn FnMut(u64),
) -> Result<(), Error> {
    let mut r = std::fs::File::open(src)
        .map_err(|e| Error::from(e).with_detail(format!("opening {}", src.display())))?;
    let mut w = std::fs::File::create(dst)
        .map_err(|e| Error::from(e).with_detail(format!("creating {}", dst.display())))?;
    let mut buf = vec![0u8; CHUNK];
    loop {
        if ctl.cancelled() {
            return Err(Error::new(ErrorCode::Cancelled));
        }
        let n = r
            .read(&mut buf)
            .map_err(|e| Error::from(e).with_detail("reading the index"))?;
        if n == 0 {
            break;
        }
        w.write_all(&buf[..n])
            .map_err(|e| Error::from(e).with_detail("writing the new index"))?;
        on_bytes(n as u64);
    }
    w.sync_all()
        .map_err(|e| Error::from(e).with_detail("writing the new index"))
}

/// After a successful start on the new location: delete the old copy's index entries and the
/// plan. Returns the finished plan (for the activity line), or `None` when there was nothing to
/// clean up. Never deletes anything unless the engine runs on the plan's target.
pub fn cleanup_after_start(anchor: &Path, running_root: &Path) -> Option<MovePlan> {
    let plan = read_plan(anchor)?;
    if plan.state != PlanState::Switched {
        return None;
    }
    if key(running_root) != key(&plan.to) {
        tracing::warn!(target: "sr_engine::indexmove", "started somewhere else than the move target; old index kept");
        return None;
    }
    if key(&plan.from) != key(&plan.to) {
        delete_entries(&plan.from);
        if key(&plan.from) != key(anchor) {
            // Our own folder; removed only if nothing else is left in it.
            let _ = std::fs::remove_dir(&plan.from);
        }
    }
    remove_plan(anchor);
    Some(plan)
}

/// The engine could not start on the new location: point back to the old copy (still intact,
/// cleanup only runs after a successful start) and drop the new one.
pub fn revert_switch(anchor: &Path) -> Option<PathBuf> {
    let plan = read_plan(anchor)?;
    if plan.state != PlanState::Switched {
        return None;
    }
    let back = if key(&plan.from) == key(anchor) {
        datadir::write_pointer(anchor, None)
    } else {
        datadir::write_pointer(anchor, Some(&plan.from))
    };
    if let Err(e) = back {
        tracing::error!(target: "sr_engine::indexmove", err = %e, "could not point back to the old index");
        return None;
    }
    discard_copy(anchor, &plan);
    remove_plan(anchor);
    Some(plan.from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_index(root: &Path) {
        std::fs::create_dir_all(root.join("lance").join("t.lance")).unwrap();
        std::fs::create_dir_all(root.join("tantivy")).unwrap();
        std::fs::write(root.join("catalog.db"), vec![7u8; 3 * CHUNK + 17]).unwrap();
        std::fs::write(root.join("lance").join("t.lance").join("a.bin"), b"vectors").unwrap();
        std::fs::write(root.join("tantivy").join("meta.json"), b"{}").unwrap();
        std::fs::write(root.join("settings.toml"), b"version = 1").unwrap();
    }

    fn plan(anchor: &Path, to: &Path) -> MovePlan {
        let p = MovePlan {
            from: anchor.to_path_buf(),
            to: to.to_path_buf(),
            state: PlanState::Pending,
            created_at: 1,
        };
        write_plan(anchor, &p).unwrap();
        p
    }

    #[test]
    fn copy_verify_switch_then_cleanup_keeps_settings() {
        let t = tempfile::tempdir().unwrap();
        let anchor = t.path().join("anchor");
        let to = t.path().join("d").join(INDEX_DIR_NAME);
        std::fs::create_dir_all(t.path().join("d")).unwrap();
        seed_index(&anchor);
        plan(&anchor, &to);
        let ctl = MoveControl::default();
        let events = std::sync::Mutex::new(Vec::new());
        let publish = |e: EngineEvent| events.lock().unwrap().push(e);
        let out = run_pending(&anchor, &ctl, &publish);
        assert_eq!(
            out,
            MoveOutcome::Switched {
                from: anchor.clone(),
                to: to.clone()
            }
        );
        assert_eq!(datadir::read_pointer(&anchor), Some(to.clone()));
        assert_eq!(
            std::fs::read(to.join("lance").join("t.lance").join("a.bin")).unwrap(),
            b"vectors"
        );
        assert!(
            !to.join("settings.toml").exists(),
            "settings stay in the anchor"
        );
        let ev = events.lock().unwrap();
        assert!(matches!(ev.last(), Some(EngineEvent::IndexMoveDone { .. })));
        assert!(ev.iter().any(
            |e| matches!(e, EngineEvent::IndexMoveProgress(p) if p.phase == MovePhase::Verifying)
        ));
        drop(ev);
        // The old copy survives until the engine runs on the new one.
        assert!(cleanup_after_start(&anchor, &anchor).is_none());
        assert!(anchor.join("catalog.db").exists());
        let done = cleanup_after_start(&anchor, &to).unwrap();
        assert_eq!(done.to, to);
        assert!(!anchor.join("catalog.db").exists() && !anchor.join("lance").exists());
        assert!(
            anchor.join("settings.toml").exists(),
            "never deletes settings"
        );
        assert!(read_plan(&anchor).is_none());
        // Layout follows the pointer.
        let l = datadir::prepare(&anchor).unwrap();
        assert_eq!(l.root, to);
        assert_eq!(l.settings, anchor.join(sr_common::config::SETTINGS_FILE));
        assert!(!l.is_default());
    }

    /// S-02 (R39): an empty folder that already exists at the target (pre-created, with an
    /// explicit grant for Users) is claimed: owner us, protected DACL, the foreign ACE gone.
    #[cfg(windows)]
    #[test]
    fn preexisting_empty_target_gets_a_private_acl() {
        let t = tempfile::tempdir().unwrap();
        let anchor = t.path().join("anchor");
        let to = t.path().join("d").join(INDEX_DIR_NAME);
        std::fs::create_dir_all(&to).unwrap();
        let sys = sr_client::win::system_dir().unwrap();
        let ok = std::process::Command::new(sys.join("icacls.exe"))
            .arg(&to)
            .args(["/grant", "*S-1-5-32-545:(OI)(CI)F", "/Q"])
            .status()
            .unwrap();
        assert!(ok.success());
        seed_index(&anchor);
        plan(&anchor, &to);
        let out = run_pending(&anchor, &MoveControl::default(), &|_| {});
        assert!(matches!(out, MoveOutcome::Switched { .. }), "{out:?}");
        let me = sr_client::win::user_sid_string().unwrap();
        let sddl = sr_client::win::path_security_sddl(&to).unwrap();
        assert!(datadir::dacl_is_private(&sddl, &me), "{sddl}");
        assert_eq!(datadir::owner_of(&sddl), Some(me.as_str()));
        // Copied files inherit the private ACL only.
        let f = sr_client::win::path_security_sddl(&to.join("catalog.db")).unwrap();
        assert!(!f.contains("BU)") && !f.contains("S-1-5-32-545"), "{f}");
    }

    /// S-02: a target folder that is not empty when the copy starts is refused and left
    /// untouched (never cleaned up as if it were ours).
    #[test]
    fn nonempty_target_is_refused_and_kept() {
        let t = tempfile::tempdir().unwrap();
        let anchor = t.path().join("anchor");
        let to = t.path().join("d").join(INDEX_DIR_NAME);
        std::fs::create_dir_all(&to).unwrap();
        std::fs::write(to.join("someone-elses.txt"), b"keep me").unwrap();
        seed_index(&anchor);
        plan(&anchor, &to);
        let out = run_pending(&anchor, &MoveControl::default(), &|_| {});
        assert!(
            matches!(
                out,
                MoveOutcome::Failed {
                    code: ErrorCode::Conflict,
                    ..
                }
            ),
            "{out:?}"
        );
        assert_eq!(
            std::fs::read(to.join("someone-elses.txt")).unwrap(),
            b"keep me"
        );
        assert!(anchor.join("catalog.db").exists());
        assert!(datadir::read_pointer(&anchor).is_none());
        assert!(read_plan(&anchor).is_none());
    }

    #[test]
    fn cancel_and_failure_leave_the_old_index() {
        let t = tempfile::tempdir().unwrap();
        let anchor = t.path().join("anchor");
        let to = t.path().join("d").join(INDEX_DIR_NAME);
        std::fs::create_dir_all(t.path().join("d")).unwrap();
        seed_index(&anchor);
        plan(&anchor, &to);
        let ctl = MoveControl::default();
        ctl.set(Some(IndexMoveProgress {
            phase: MovePhase::Waiting,
            done_bytes: 0,
            total_bytes: 0,
            from: String::new(),
            to: String::new(),
        }));
        assert!(ctl.cancel());
        let out = run_pending(&anchor, &ctl, &|_| {});
        assert!(matches!(
            out,
            MoveOutcome::Failed {
                code: ErrorCode::Cancelled,
                ..
            }
        ));
        assert!(datadir::read_pointer(&anchor).is_none());
        assert!(!to.exists(), "the partial copy is removed");
        assert!(anchor.join("catalog.db").exists());
        assert!(read_plan(&anchor).is_none());
    }

    #[test]
    fn revert_points_back_and_drops_the_copy() {
        let t = tempfile::tempdir().unwrap();
        let anchor = t.path().join("anchor");
        let to = t.path().join("d").join(INDEX_DIR_NAME);
        std::fs::create_dir_all(t.path().join("d")).unwrap();
        seed_index(&anchor);
        plan(&anchor, &to);
        let ctl = MoveControl::default();
        assert!(matches!(
            run_pending(&anchor, &ctl, &|_| {}),
            MoveOutcome::Switched { .. }
        ));
        assert_eq!(revert_switch(&anchor), Some(anchor.clone()));
        assert!(datadir::read_pointer(&anchor).is_none());
        assert!(!to.exists());
        assert!(anchor.join("catalog.db").exists());
    }

    #[test]
    fn targets_are_checked() {
        let t = tempfile::tempdir().unwrap();
        let anchor = t.path().join("anchor");
        seed_index(&anchor);
        let folder = t.path().join("docs");
        std::fs::create_dir_all(&folder).unwrap();
        let app = t.path().join("app");
        std::fs::create_dir_all(&app).unwrap();
        let free = t.path().join("free");
        std::fs::create_dir_all(&free).unwrap();
        let roots = vec![folder.clone()];
        let run = |target: &Path| {
            check(&CheckInput {
                anchor: &anchor,
                current: &anchor,
                target: &target.to_string_lossy(),
                folder_roots: &roots,
                app_dir: &app,
                size: 10,
            })
            .unwrap()
        };
        let ok = run(&free);
        assert!(ok.ok, "{ok:?}");
        assert_eq!(
            ok.index_path,
            free.join(INDEX_DIR_NAME).display().to_string()
        );
        assert_eq!(ok.need_bytes, needed_bytes(10));
        assert_eq!(run(&folder).problem, Some(MoveProblem::InsideFolder));
        assert_eq!(
            run(&folder.join("sub")).problem,
            Some(MoveProblem::InsideFolder)
        );
        assert_eq!(run(&app).problem, Some(MoveProblem::InsideApp));
        assert_eq!(run(&anchor).problem, Some(MoveProblem::SameAsCurrent));
        std::fs::create_dir_all(free.join(INDEX_DIR_NAME).join("x")).unwrap();
        assert_eq!(run(&free).problem, Some(MoveProblem::NotEmpty));
        #[cfg(windows)]
        assert_eq!(
            run(Path::new(r"\\server\share\x")).problem,
            Some(MoveProblem::NotLocal)
        );
        #[cfg(target_os = "macos")]
        {
            use std::os::unix::fs::PermissionsExt;
            // The volume's mount point is the "drive".
            assert!(
                !ok.drive.is_empty() && Path::new(&ok.drive).is_absolute(),
                "{ok:?}"
            );
            // R52: a folder others may write to (no sticky bit) on the way is refused; with the
            // sticky bit (like /tmp) it is fine.
            let shared = t.path().join("shared");
            std::fs::create_dir_all(shared.join("in")).unwrap();
            std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
            assert_eq!(
                run(&shared.join("in")).problem,
                Some(MoveProblem::NotWritable)
            );
            assert_eq!(run(&shared).problem, Some(MoveProblem::NotWritable));
            std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o1777)).unwrap();
            let r = run(&shared.join("in"));
            assert!(r.ok, "{r:?}");
            assert!(local_volume(t.path()));
        }
        let long = t.path().join("y".repeat(150));
        assert_eq!(run(&long).problem, Some(MoveProblem::TooLong));
        assert_eq!(
            check(&CheckInput {
                anchor: &anchor,
                current: &anchor,
                target: "relative",
                folder_roots: &roots,
                app_dir: &app,
                size: 0,
            })
            .unwrap_err()
            .code,
            ErrorCode::InvalidInput
        );
    }
}
