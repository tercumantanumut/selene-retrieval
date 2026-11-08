//! The one event set (docs/02-architecture.md §2.5 "Events"; review item M7).
//!
//! * Every event is wrapped as [`Seq<T>`] `{seq, ts_ms, payload}`; `seq` is strictly monotonic
//!   per daemon run and assigned under the same lock that appends to the replay ring, so ring
//!   order equals seq order.
//! * Snapshots ([`Snapshot<T>`], e.g. `Snapshot<EngineStatus>`) carry the `seq` they reflect and
//!   the [`StreamId`] of the daemon run, so a client applies only events with `seq > snapshot.seq`.
//! * [`EventLog`] is the bounded replay ring behind `subscribe(cursor)`: it either replays the
//!   missed events or tells the client to re-snapshot. A cursor from another daemon run (different
//!   `stream`) always re-snapshots, because `seq` restarts at 1 on every run.
//!
//! Progress is push-only (P1.5): nothing here is meant to be polled.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{ClientKind, ErrorCode, Purpose, UserMessage};
use crate::ids::{ApprovalId, CollectionId, FileId, FolderId, GroupId, StreamId, VectorIndexId};
use crate::tier::{EpActive, EpPlan, GpuClass, Tier};

/// Default ring capacity (docs/00 §0.A M7).
pub const DEFAULT_RING_CAPACITY: usize = 4096;

/// Sequence number. `0` means "nothing seen yet"; the first event is `1`.
pub type SeqNo = u64;

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// A sequenced event envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Seq<T> {
    pub seq: SeqNo,
    pub ts_ms: u64,
    pub payload: T,
}

/// A snapshot together with the last `seq` it already reflects.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Snapshot<T> {
    pub stream: StreamId,
    pub seq: SeqNo,
    pub value: T,
}

impl<T> Snapshot<T> {
    /// Where to resume the event subscription after applying this snapshot.
    pub fn cursor(&self) -> Cursor {
        Cursor {
            stream: self.stream,
            seq: self.seq,
        }
    }
}

/// Resume position of a subscriber: the daemon run and the last `seq` it has applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Cursor {
    pub stream: StreamId,
    pub seq: SeqNo,
}

/// Strictly monotonic counter.
#[derive(Debug, Default)]
pub struct SeqCounter(AtomicU64);

impl SeqCounter {
    pub const fn new() -> Self {
        Self(AtomicU64::new(0))
    }
    /// Next sequence number (1, 2, 3, ...).
    pub fn next(&self) -> SeqNo {
        self.0.fetch_add(1, Ordering::AcqRel) + 1
    }
    /// Last issued sequence number (0 if none).
    pub fn current(&self) -> SeqNo {
        self.0.load(Ordering::Acquire)
    }
}

/// Result of `subscribe(from_seq)`.
#[derive(Debug, Clone, PartialEq)]
pub enum Replay<T> {
    /// Every event with `seq > from_seq`, in order (possibly empty).
    Events(Vec<Seq<T>>),
    /// The ring no longer holds everything after the cursor, or the cursor is from another daemon
    /// run: fetch a fresh snapshot.
    Resnapshot,
}

struct LogInner<T> {
    last: SeqNo,
    ring: VecDeque<Seq<T>>,
}

/// Sequencer + bounded replay ring.
pub struct EventLog<T> {
    stream: StreamId,
    cap: usize,
    inner: Mutex<LogInner<T>>,
}

impl<T: Clone> EventLog<T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            stream: StreamId::new(),
            cap: capacity.max(1),
            inner: Mutex::new(LogInner {
                last: 0,
                ring: VecDeque::with_capacity(capacity.max(1)),
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LogInner<T>> {
        // A panic while holding the lock cannot leave the ring inconsistent (push/pop only).
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Assign the next seq, append to the ring and return the envelope to broadcast.
    pub fn publish(&self, payload: T) -> Seq<T> {
        let mut g = self.lock();
        g.last += 1;
        let ev = Seq {
            seq: g.last,
            ts_ms: now_ms(),
            payload,
        };
        if g.ring.len() == self.cap {
            g.ring.pop_front();
        }
        g.ring.push_back(ev.clone());
        ev
    }

    /// Last published seq.
    pub fn last_seq(&self) -> SeqNo {
        self.lock().last
    }

    /// This daemon run's stream id.
    pub fn stream(&self) -> StreamId {
        self.stream
    }

    /// Cursor at the newest event (what a fresh subscriber without a snapshot starts from).
    pub fn cursor(&self) -> Cursor {
        Cursor {
            stream: self.stream,
            seq: self.last_seq(),
        }
    }

    /// Take a snapshot consistently with the sequence: `f` runs under the log lock, so no event
    /// can be published between reading state and reading `seq`.
    pub fn snapshot<S>(&self, f: impl FnOnce() -> S) -> Snapshot<S> {
        let g = self.lock();
        Snapshot {
            stream: self.stream,
            seq: g.last,
            value: f(),
        }
    }

    /// Events after `cursor.seq`, or `Resnapshot` if they are no longer all available or the
    /// cursor belongs to another daemon run.
    pub fn replay_since(&self, cursor: Cursor) -> Replay<T> {
        if cursor.stream != self.stream {
            return Replay::Resnapshot;
        }
        let from_seq = cursor.seq;
        let g = self.lock();
        if from_seq > g.last {
            return Replay::Resnapshot;
        }
        if from_seq == g.last {
            return Replay::Events(Vec::new());
        }
        let oldest = g.ring.front().map_or(g.last + 1, |e| e.seq);
        if from_seq + 1 < oldest {
            return Replay::Resnapshot;
        }
        Replay::Events(
            g.ring
                .iter()
                .filter(|e| e.seq > from_seq)
                .cloned()
                .collect(),
        )
    }
}

/// Folder status (docs/02-architecture.md §2.4 / review M5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FolderStatus {
    Pending,
    Scanning,
    Indexing,
    Synced,
    SyncedWithErrors,
    Partial,
    Paused,
    Unavailable,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HostState {
    Starting,
    Ready,
    Draining,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StoreKind {
    Lance,
    Tantivy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    Ok,
    Degraded,
    Down,
}

/// Every event the engine pushes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EngineEvent {
    FolderStatusChanged {
        folder_id: FolderId,
        status: FolderStatus,
        reason: Option<ErrorCode>,
    },
    ScanProgress {
        folder_id: FolderId,
        done: u64,
        total_est: Option<u64>,
    },
    IndexProgress {
        folder_id: FolderId,
        files_done: u64,
        files_total: u64,
        eta_s: Option<u64>,
    },
    FileFailed {
        folder_id: FolderId,
        file_id: FileId,
        code: ErrorCode,
    },
    QueueDepth {
        queued: u64,
        leased: u64,
    },
    IndexCommitted {
        store: StoreKind,
        collection_id: CollectionId,
        seq: u64,
    },
    ProviderHealth {
        provider: String,
        health: Health,
    },
    ApprovalRequested {
        approval_id: ApprovalId,
        client: ClientKind,
    },
    PurgeCompleted {
        folder_id: FolderId,
    },
    /// `tier.detected`: every daemon start and on display-adapter change (§2.8).
    #[serde(rename = "tier.detected")]
    TierDetected {
        recommended: Tier,
        active: Tier,
        override_: Option<Tier>,
        ep: EpPlan,
        gpu_class: GpuClass,
        /// Details only.
        reasons: Vec<String>,
    },
    /// `ep.changed`: diagnostics only, silent in Basic.
    #[serde(rename = "ep.changed")]
    EpChanged {
        from: EpActive,
        to: EpActive,
        reason_code: EpChangeReason,
    },
    /// `model.download.progress`: at most every 250 ms.
    #[serde(rename = "model.download.progress")]
    ModelDownloadProgress {
        model_id: String,
        bytes: u64,
        total: u64,
    },
    #[serde(rename = "model.download.completed")]
    ModelDownloadCompleted {
        model_id: String,
        sha256: String,
    },
    /// `code` is one of ModelDownloadFailed | ModelIntegrity | Offline | NetworkBlocked | DiskFull.
    #[serde(rename = "model.download.failed")]
    ModelDownloadFailed {
        model_id: String,
        code: ErrorCode,
        resumable: bool,
    },
    #[serde(rename = "reembed.started")]
    ReembedStarted {
        index_id: VectorIndexId,
        model_id: String,
        dims: u32,
        total: u64,
        resumed_from: u64,
    },
    /// Throttled to 4 Hz.
    #[serde(rename = "reembed.progress")]
    ReembedProgress {
        index_id: VectorIndexId,
        done: u64,
        total: u64,
        eta_s: Option<u64>,
    },
    #[serde(rename = "reembed.failed")]
    ReembedFailed {
        index_id: VectorIndexId,
        code: ErrorCode,
        resumable: bool,
    },
    #[serde(rename = "index.swapped")]
    IndexSwapped {
        collection_id: CollectionId,
        old_index_id: Option<VectorIndexId>,
        new_index_id: VectorIndexId,
        model_id: String,
        dims: u32,
    },
    ClientConnected {
        client: ClientKind,
    },
    SettingsChanged,
    HostState {
        state: HostState,
    },
    /// Restart catch-up progress (Amendment 4.24): outbox work left by the last run is applied in
    /// the background while search serves the last committed state. `done == total` ends it.
    /// Pushed after every committed batch of the catch-up.
    CatchUpProgress {
        done: u64,
        total: u64,
    },
    /// A notice for the user (also kept in [`EngineStatus::notices`] until the next start), e.g.
    /// "Some settings were reset" (R-UX19).
    Notice {
        notice: Notice,
    },
    /// `gpu.fullscreen_wait` (R-UX32, M3-Q): GPU indexing waits because a full-screen app is in
    /// front (`waiting: true`, status "Waiting while a full-screen app is open"), or stopped
    /// waiting (`false`: the app closed, or after 5 minutes indexing went on on the processor).
    #[serde(rename = "gpu.fullscreen_wait")]
    GpuFullscreenWait {
        waiting: bool,
    },
    /// `consent.needed` (M3-Q): a call was blocked because the user hasn't allowed `purpose` for
    /// `provider` (provider **id**) yet; also kept in [`EngineStatus::consent_needed`].
    #[serde(rename = "consent.needed")]
    ConsentNeeded {
        provider: String,
        purpose: Purpose,
    },
    /// `consent.changed` (M3-Q): after `consent.set` (answer covers iterate: one event each).
    #[serde(rename = "consent.changed")]
    ConsentChanged {
        provider: String,
        purpose: Purpose,
        allowed: bool,
    },
    /// `groups.changed` (M4): a folder group was created, renamed, deleted, or a folder moved.
    #[serde(rename = "groups.changed")]
    GroupsChanged,
    /// `clients.changed` (M4): a connected app appeared, changed scope / name / token, was
    /// forgotten, or its sessions / last-seen moved (last-seen throttled to once a minute).
    #[serde(rename = "clients.changed")]
    ClientsChanged {
        client_id: Option<String>,
    },
    /// `http.state` (M4): the opt-in HTTP listener started, stopped or failed.
    #[serde(rename = "http.state")]
    HttpState(crate::api::HttpStatus),
    /// `request.progress` (M4): phase of an app `answer` / `enhance_prompt` call with a
    /// `request_id`.
    #[serde(rename = "request.progress")]
    RequestProgress(crate::api::RequestProgress),
    /// `activity.added` (M4, page 30).
    #[serde(rename = "activity.added")]
    ActivityAdded {
        entry: crate::api::ActivityEntry,
    },
    /// `egress.recorded` (M4, page 30): one audited provider request.
    #[serde(rename = "egress.recorded")]
    EgressRecorded {
        row: crate::api::EgressRow,
    },
    /// `index.move.progress` (M4, page 32), at most 4 Hz.
    #[serde(rename = "index.move.progress")]
    IndexMoveProgress(crate::api::IndexMoveProgress),
    /// `index.move.done` (M4): the index now lives at `path`.
    #[serde(rename = "index.move.done")]
    IndexMoveDone {
        path: String,
    },
    /// `index.move.failed` (M4): nothing was moved; the index stays at `kept_path`.
    #[serde(rename = "index.move.failed")]
    IndexMoveFailed {
        code: ErrorCode,
        kept_path: String,
    },
    /// `cli_signin.changed` (M5-C, Amendments 10.2 / 10.3): a CLI sign-in provider's state
    /// changed (its sign-in window closed, a check found a new state). Never carries a token.
    #[serde(rename = "cli_signin.changed")]
    CliSigninChanged(crate::api::CliSigninStatus),
}

/// A blocked purpose waiting for the user's consent (`EngineStatus::consent_needed`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ConsentRequest {
    /// Provider id (`openrouter`).
    pub provider: String,
    pub purpose: Purpose,
}

/// "Catching up after restart" (Amendment 4.24). Units are pending index updates (outbox ops).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CatchUp {
    pub done: u64,
    pub total: u64,
}

impl CatchUp {
    /// Whole percent, 0..=100.
    pub fn percent(&self) -> u64 {
        // `total == 0` → nothing to catch up → 100 %.
        self.done
            .min(self.total)
            .saturating_mul(100)
            .checked_div(self.total)
            .unwrap_or(100)
    }
}

/// A plain notice shown to the user (tray banner, `sr://status`), rendered by the one renderer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Notice {
    pub code: ErrorCode,
    /// Title, body and the one action (e.g. "Some settings were reset" → "Review settings").
    pub message: UserMessage,
    /// Details only: the dotted settings paths that were reset (`SettingsInvalid`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<String>,
}

/// Why the execution provider changed (`ep.changed`; diagnostics only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EpChangeReason {
    ProbeResult,
    RegistrationFailed,
    CanaryFailed,
    DeviceRemoved,
    UserSwitch,
}

/// Per-folder line of the status snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FolderStatusSummary {
    pub folder_id: FolderId,
    pub display_name: String,
    pub status: FolderStatus,
    pub files_indexed: u64,
    pub files_total: u64,
    /// The folder's root (shown under the name; M2-A R3, so no `folders.list` follow-up is needed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Files that couldn't be read ("couldn't be read" list; M2-A R3).
    #[serde(default)]
    pub files_skipped: u64,
    /// Plain note when the live watcher can't serve the folder (OQ-38, `sr_sync::watch_note`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch_note: Option<String>,
    /// M5-E: "Some files are online-only in OneDrive; …" (`sr_common::api::ONLINE_ONLY_NOTE`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud_note: Option<String>,
    /// M5-E: how many files were skipped because they are online-only in OneDrive (0 when the
    /// folder includes online-only files on purpose).
    #[serde(default, skip_serializing_if = "crate::api::is_zero")]
    pub online_only_skipped: u64,
    /// Removed by the user; shown as "Removing…" until its data is purged (OQ-18, M3-Q).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub removing: bool,
    /// Folder group (M4, page 31).
    #[serde(default)]
    pub group_id: GroupId,
    /// The user stopped the last run and no full check finished since (OQ-37, M4).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stopped: bool,
    /// The folder was set to only list its files, a removed mode (Amendment 17): paused until
    /// the user chooses to search its contents or remove it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub was_files_only: bool,
}

impl Default for FolderStatusSummary {
    /// Build literals with `..FolderStatusSummary::default()` so new fields don't break callers.
    fn default() -> Self {
        Self {
            folder_id: FolderId::from_bytes([0; 16]),
            display_name: String::new(),
            status: FolderStatus::Pending,
            files_indexed: 0,
            files_total: 0,
            path: None,
            files_skipped: 0,
            watch_note: None,
            cloud_note: None,
            online_only_skipped: 0,
            removing: false,
            group_id: GroupId::from_bytes([0; 16]),
            stopped: false,
            was_files_only: false,
        }
    }
}

/// Search model tier in the status snapshot (M2-A R2: `tier.detected` is pushed only at start, so
/// an app attaching later reads it here).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TierSummary {
    /// Tier of the active index's model.
    pub active: Tier,
    /// Detected recommendation for this computer.
    pub recommended: Tier,
    /// Indexing runs on the graphics card.
    pub gpu: bool,
    /// The active index uses the multilingual model (M4-E; `active` then reads `minimum`, its
    /// resource class, M3-Q deviation 6).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub multilingual: bool,
}

/// Status snapshot; `summary` is the plain one-liner shown in the tray / `sr://status`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EngineStatus {
    pub host: HostState,
    pub folders: Vec<FolderStatusSummary>,
    pub queued_jobs: u64,
    pub summary: String,
    /// Set while the restart catch-up runs (Amendment 4.24); `summary` then says
    /// "Catching up after restart (n%)".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catch_up: Option<CatchUp>,
    /// Notices for the user, newest last (R-UX19: "Some settings were reset").
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notices: Vec<Notice>,
    /// Active and recommended search model tier (M2-A R2); `None` while starting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<TierSummary>,
    /// GPU indexing waits behind a full-screen app (R-UX32); `summary` then says so.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub waiting_for_fullscreen: bool,
    /// The last call blocked for missing consent, until the user decides (M3-Q, R-Q4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consent_needed: Option<ConsentRequest>,
    /// The search index is being moved (M4, page 32); `summary` then says so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index_move: Option<crate::api::IndexMoveProgress>,
    /// M5-E (R34): the moved index is missing; the service waits in its recovery mode and
    /// `summary` says "Search index not found". The app offers the `index_location.recover`
    /// choices (and reconnecting the drive starts the service by itself).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index_missing: Option<crate::api::IndexMissingInfo>,
    /// M5-E: settings an administrator locked (`policy.toml` / registry policy). Each entry is a
    /// settings path plus the plain line "Managed by your organisation"; empty when unmanaged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub managed: Vec<crate::config::policy::ManagedField>,
    /// M5-E S-04 (R40): the file reader runs with the low-integrity fallback because the
    /// AppContainer is unsupported on this PC. The app shows the quiet line
    /// [`REDUCED_PROTECTION_NOTE`] (Settings → System and diagnostics).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reduced_protection: bool,
}

/// S-04: the plain line for [`EngineStatus::reduced_protection`].
pub const REDUCED_PROTECTION_NOTE: &str = "File reader running with reduced protection";

impl Default for EngineStatus {
    /// "Starting…" with nothing known yet. Build literals with `..EngineStatus::default()` so new
    /// fields don't break callers.
    fn default() -> Self {
        Self {
            host: HostState::Starting,
            folders: Vec::new(),
            queued_jobs: 0,
            summary: String::new(),
            catch_up: None,
            notices: Vec::new(),
            tier: None,
            waiting_for_fullscreen: false,
            consent_needed: None,
            index_move: None,
            index_missing: None,
            managed: Vec::new(),
            reduced_protection: false,
        }
    }
}

impl EngineStatus {
    /// Plain-language summary (U6, U25): never says "daemon".
    pub fn plain_summary(&self) -> String {
        // R34: the recovery mode wins over everything (the engine isn't running).
        if self.index_missing.is_some() {
            return "Search index not found".into();
        }
        // Page 32: the move runs before the engine starts, so it wins over "Starting…".
        if let Some(m) = &self.index_move
            && !matches!(m.phase, crate::api::MovePhase::Cleaning)
        {
            return format!("Moving the search index ({}%)", m.percent());
        }
        if self.host == HostState::Starting {
            return "Starting…".into();
        }
        if let Some(c) = self.catch_up
            && c.done < c.total
        {
            return format!("Catching up after restart ({}%)", c.percent());
        }
        // Progress counts only folders that are still at work. A settled folder's skipped and
        // unreadable files are never "left to index" (owner report 2026-10-20: "Indexing 482 of
        // 505 files" stayed on with nothing queued and the folder `synced_with_errors`), and in a
        // busy folder a file that couldn't be read is done, not waiting.
        let (done, total) = self
            .folders
            .iter()
            .filter(|f| {
                matches!(
                    f.status,
                    FolderStatus::Indexing | FolderStatus::Pending | FolderStatus::Scanning
                )
            })
            .fold((0, 0), |(d, t), f| {
                (
                    d + (f.files_indexed + f.files_skipped).min(f.files_total),
                    t + f.files_total,
                )
            });
        // R-UX32: an indexing wait behind a full-screen app is said plainly, never silent.
        if self.waiting_for_fullscreen && done < total {
            return "Waiting while a full-screen app is open".into();
        }
        if self
            .folders
            .iter()
            .all(|f| f.status == FolderStatus::Paused)
            && !self.folders.is_empty()
        {
            "Paused".into()
        } else if done < total {
            format!("Indexing {done} of {total} files")
        } else if self.folders.is_empty() {
            "No folders yet".into()
        } else {
            "Up to date".into()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seq_is_strictly_monotonic_across_threads() {
        let c = std::sync::Arc::new(SeqCounter::new());
        let hs: Vec<_> = (0..8)
            .map(|_| {
                let c = c.clone();
                std::thread::spawn(move || (0..1000).map(|_| c.next()).collect::<Vec<_>>())
            })
            .collect();
        let mut all: Vec<u64> = hs.into_iter().flat_map(|h| h.join().unwrap()).collect();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), 8000);
        assert_eq!(c.current(), 8000);
    }

    #[test]
    fn replay_and_resnapshot() {
        let log = EventLog::new(4);
        for _ in 0..3 {
            log.publish(EngineEvent::SettingsChanged);
        }
        let at = |seq| Cursor {
            stream: log.stream(),
            seq,
        };
        match log.replay_since(at(1)) {
            Replay::Events(v) => assert_eq!(v.iter().map(|e| e.seq).collect::<Vec<_>>(), [2, 3]),
            Replay::Resnapshot => panic!("expected events"),
        }
        assert_eq!(log.replay_since(at(3)), Replay::Events(vec![]));
        assert_eq!(
            log.replay_since(at(99)),
            Replay::Resnapshot,
            "future seq = other daemon run"
        );
        let other_run = Cursor {
            stream: StreamId::new(),
            seq: 1,
        };
        assert_eq!(
            log.replay_since(other_run),
            Replay::Resnapshot,
            "a low seq from a previous run must not be mixed with this run's events"
        );
        for _ in 0..5 {
            log.publish(EngineEvent::SettingsChanged);
        }
        // ring holds 5..=8
        assert_eq!(log.replay_since(at(2)), Replay::Resnapshot);
        match log.replay_since(at(4)) {
            Replay::Events(v) => assert_eq!(v.first().map(|e| e.seq), Some(5)),
            Replay::Resnapshot => panic!("4 is still replayable"),
        }
    }

    #[test]
    fn snapshot_carries_seq() {
        let log = EventLog::new(16);
        log.publish(EngineEvent::HostState {
            state: HostState::Ready,
        });
        let s = log.snapshot(|| "state");
        assert_eq!(s.seq, 1);
        assert_eq!(s.cursor(), log.cursor());
        let e = log.publish(EngineEvent::SettingsChanged);
        assert!(e.seq > s.seq);
    }

    #[test]
    fn events_serialise_with_type_tag() {
        let e = EngineEvent::QueueDepth {
            queued: 1,
            leased: 0,
        };
        let j = serde_json::to_value(&e).unwrap();
        assert_eq!(j["type"], "queue_depth");
    }

    #[test]
    fn tier_and_model_events_use_dotted_wire_names() {
        let e = EngineEvent::TierDetected {
            recommended: Tier::Standard,
            active: Tier::Minimum,
            override_: None,
            ep: EpPlan::DirectMl { dxgi_index: 0 },
            gpu_class: GpuClass::Dedicated,
            reasons: vec![],
        };
        let j = serde_json::to_value(&e).unwrap();
        assert_eq!(j["type"], "tier.detected");
        assert_eq!(j["ep"]["kind"], "direct_ml");
        let back: EngineEvent = serde_json::from_value(j).unwrap();
        assert_eq!(back, e);
        let e = EngineEvent::IndexSwapped {
            collection_id: CollectionId::new(),
            old_index_id: None,
            new_index_id: VectorIndexId::new(),
            model_id: "Snowflake/snowflake-arctic-embed-m-v1.5".into(),
            dims: 768,
        };
        assert_eq!(serde_json::to_value(&e).unwrap()["type"], "index.swapped");
        let e = EngineEvent::ModelDownloadFailed {
            model_id: "m".into(),
            code: ErrorCode::ModelIntegrity,
            resumable: false,
        };
        assert_eq!(
            serde_json::to_value(&e).unwrap()["type"],
            "model.download.failed"
        );
    }

    #[test]
    fn m4_events_use_dotted_wire_names() {
        let e = EngineEvent::GroupsChanged;
        assert_eq!(
            serde_json::to_value(&e).unwrap(),
            serde_json::json!({"type": "groups.changed"})
        );
        let e = EngineEvent::HttpState(crate::api::HttpStatus {
            enabled: true,
            running: true,
            port: Some(41234),
            url: Some("http://127.0.0.1:41234/mcp".into()),
            error: None,
            tools: 1,
        });
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["type"], "http.state");
        assert_eq!(v["port"], 41234);
        assert_eq!(serde_json::from_value::<EngineEvent>(v).unwrap(), e);
        let e = EngineEvent::RequestProgress(crate::api::RequestProgress {
            request_id: "r1".into(),
            kind: crate::api::RequestKind::Answer,
            phase: crate::api::RequestPhase::SearchingAgain,
            round: Some(1),
            max: Some(2),
        });
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["type"], "request.progress");
        assert_eq!(v["phase"], "searching_again");
        assert_eq!(serde_json::from_value::<EngineEvent>(v).unwrap(), e);
        let e = EngineEvent::IndexMoveFailed {
            code: ErrorCode::DiskFull,
            kept_path: r"C:\x".into(),
        };
        assert_eq!(
            serde_json::to_value(&e).unwrap()["type"],
            "index.move.failed"
        );
        let e = EngineEvent::ClientsChanged { client_id: None };
        assert_eq!(serde_json::to_value(&e).unwrap()["type"], "clients.changed");
    }

    #[test]
    fn moving_summary_wins_over_starting() {
        let s = EngineStatus {
            index_move: Some(crate::api::IndexMoveProgress {
                phase: crate::api::MovePhase::Copying,
                done_bytes: 1,
                total_bytes: 4,
                from: r"C:\a".into(),
                to: r"D:\b".into(),
            }),
            ..EngineStatus::default()
        };
        assert_eq!(s.plain_summary(), "Moving the search index (25%)");
        let old = serde_json::json!({
            "host": "ready", "folders": [{"folder_id": FolderId::new(), "display_name": "x",
            "status": "synced", "files_indexed": 1, "files_total": 1}],
            "queued_jobs": 0, "summary": "Up to date"
        });
        let back: EngineStatus = serde_json::from_value(old).unwrap();
        assert!(back.index_move.is_none() && !back.folders[0].stopped);
    }

    /// Owner report 2026-10-20: a finished folder with skipped and unreadable files (482 indexed,
    /// 12 skipped, 11 couldn't be read, nothing queued) showed "Indexing 482 of 505 files" for
    /// good. Only folders at work count, and a file that couldn't be read is done.
    #[test]
    fn settled_folders_with_skipped_files_never_read_as_indexing() {
        let line = |status, indexed, total, skipped| FolderStatusSummary {
            folder_id: FolderId::new(),
            display_name: "repo".into(),
            status,
            files_indexed: indexed,
            files_total: total,
            files_skipped: skipped,
            ..FolderStatusSummary::default()
        };
        let mut s = EngineStatus {
            host: HostState::Ready,
            folders: vec![line(FolderStatus::SyncedWithErrors, 482, 505, 11)],
            ..EngineStatus::default()
        };
        assert_eq!(s.plain_summary(), "Up to date");
        s.waiting_for_fullscreen = true;
        assert_eq!(s.plain_summary(), "Up to date");
        s.waiting_for_fullscreen = false;
        // Another folder at work: only its own files count, its unreadable ones as done.
        s.folders.push(line(FolderStatus::Indexing, 40, 100, 5));
        assert_eq!(s.plain_summary(), "Indexing 45 of 100 files");
        s.folders[1].files_indexed = 95;
        assert_eq!(s.plain_summary(), "Up to date");
    }

    #[test]
    fn summary_is_plain() {
        let mut s = EngineStatus {
            host: HostState::Ready,
            ..EngineStatus::default()
        };
        assert_eq!(s.plain_summary(), "No folders yet");
        s.folders.push(FolderStatusSummary {
            folder_id: FolderId::new(),
            display_name: "Docs".into(),
            status: FolderStatus::Indexing,
            files_indexed: 340,
            files_total: 2100,
            ..FolderStatusSummary::default()
        });
        assert_eq!(s.plain_summary(), "Indexing 340 of 2100 files");
        s.catch_up = Some(CatchUp {
            done: 25,
            total: 100,
        });
        assert_eq!(s.plain_summary(), "Catching up after restart (25%)");
        s.catch_up = Some(CatchUp {
            done: 100,
            total: 100,
        });
        assert_eq!(s.plain_summary(), "Indexing 340 of 2100 files");
        // R-UX32: the full-screen wait is said plainly while there is work, never when idle.
        s.waiting_for_fullscreen = true;
        assert_eq!(s.plain_summary(), "Waiting while a full-screen app is open");
        s.folders[0].files_indexed = 2100;
        assert_eq!(s.plain_summary(), "Up to date");
        let w = serde_json::to_value(EngineEvent::GpuFullscreenWait { waiting: true }).unwrap();
        assert_eq!(
            w,
            serde_json::json!({"type": "gpu.fullscreen_wait", "waiting": true})
        );
        let c = serde_json::to_value(EngineEvent::ConsentNeeded {
            provider: "openrouter".into(),
            purpose: crate::error::Purpose::Answer,
        })
        .unwrap();
        assert_eq!(
            c,
            serde_json::json!({"type": "consent.needed", "provider": "openrouter", "purpose": "answer"})
        );
        // New fields are optional on the wire (older snapshots still parse).
        let v = serde_json::json!({
            "host": "ready", "folders": [], "queued_jobs": 0, "summary": "Up to date"
        });
        let old: EngineStatus = serde_json::from_value(v).unwrap();
        assert!(old.catch_up.is_none() && old.notices.is_empty());
    }
}
