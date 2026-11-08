//! Local view state: a status snapshot plus the events pushed after it (never polled).
//!
//! `ViewState` is pure (no I/O) so the reducer is unit-tested; the daemon link feeds it
//! `SubItem`s and pushes `view()` to the UI and the tray.

use std::collections::BTreeMap;

use serde_json::Value;
use sr_common::error::Error;
use sr_common::events::{
    CatchUp, EngineEvent, EngineStatus, FolderStatus, FolderStatusSummary, Health, HostState,
    Notice,
};
use sr_common::ids::FolderId;
use sr_common::tier::{EpPlan, Tier, TierDecision};

use sr_common::api::{IndexMoveProgress, MovePhase, RequestKind, RequestPhase, RequestProgress};

use crate::policy::{self, IndexMissingView, PolicyView};

use crate::dto::{
    AppView, ApprovalView, ConsentPurpose, ConsentRequest, FolderNote, FolderNoteKind, FolderState,
    FolderView, IndexMoveView, ModelChangeView, ModelStep, Overall, ProviderHealthView,
    ServiceSignal, TierName, TierView, UiError,
};

/// Wire names of M3 additions that `sr_common` doesn't type yet. They are read from the raw JSON
/// so the app works with the service before and after they land; requested from M3-Q in
/// `docs/research/m3-u-progress.md` (R-Q1…R-Q4). Older services simply never send them.
pub mod wire {
    /// `FolderStatusSummary` / `FolderInfo`: `sr_sync::watch_note` text (OQ-38).
    pub const WATCH_NOTE: &str = "watch_note";
    /// `FolderStatusSummary` / `FolderInfo`: removed, purge not completed yet (OQ-18).
    pub const REMOVING: &str = "removing";
    /// `EngineStatus`: GPU indexing waits behind a full-screen app (R-UX32).
    pub const WAITING_FOR_FULLSCREEN: &str = "waiting_for_fullscreen";
    /// Event `{"type": "gpu.fullscreen_wait", "waiting": bool}` (R-UX32).
    pub const EV_FULLSCREEN_WAIT: &str = "gpu.fullscreen_wait";
    /// `EngineStatus`: `{"provider", "purpose"}` of a consent the service is waiting for.
    pub const CONSENT_NEEDED: &str = "consent_needed";
    /// Event `{"type": "consent.needed", "provider", "purpose"}`.
    pub const EV_CONSENT_NEEDED: &str = "consent.needed";
    /// Event `{"type": "consent.changed", "provider", "purpose", "allowed"}`.
    pub const EV_CONSENT_CHANGED: &str = "consent.changed";
}

/// Status line while GPU indexing waits behind a full-screen app (R-UX32, exact copy).
pub const FULLSCREEN_WAIT: &str = "Waiting while a full-screen app is open";

/// Status line of a folder paused because it was set to only list its files, a removed mode
/// (Amendment 17); its card asks what to do.
pub const WAS_FILES_ONLY_LINE: &str = "Paused until you choose what to do";

#[derive(Debug, Clone, Default)]
struct Folder {
    name: String,
    path: Option<String>,
    status: Option<FolderStatus>,
    done: u64,
    total: u64,
    skipped: u64,
    eta_s: Option<u64>,
    scan_done: Option<u64>,
    /// Network-drive note from the service (OQ-38).
    note: Option<String>,
    /// Removed; shown as "Removing…" until `PurgeCompleted` (OQ-18).
    removing: bool,
    /// Folder group (M4, page 31).
    group_id: Option<String>,
    /// The user pressed Stop and no full check finished since (OQ-37, M4 contract §10).
    stopped: bool,
    /// Paused because it was set to only list its files, a removed mode (Amendment 17).
    was_files_only: bool,
    /// M5: files skipped as online-only OneDrive files (M5-E `online_only_skipped`, `policy::wire`).
    online_only_skipped: u64,
    /// M5: the service's OneDrive note (`cloud_note`, published by M5-E).
    cloud_note: Option<String>,
}

/// What the app knows; see the module docs.
#[derive(Debug, Default)]
pub struct ViewState {
    connected: bool,
    host: Option<HostState>,
    folders: BTreeMap<String, Folder>,
    queued_jobs: u64,
    problem: Option<UiError>,
    tier: Option<TierView>,
    reconnects: u32,
    /// Restart catch-up (Amendment 4.24): search works, updates are being re-applied.
    catch_up: Option<CatchUp>,
    /// User notices from the service (R-UX19: "Some settings were reset").
    notices: Vec<Notice>,
    /// [Use it] progress: download, re-index, done (pushed events).
    model_change: Option<ModelChangeView>,
    approvals: Vec<ApprovalView>,
    /// A folder id appeared in an event that the last snapshot didn't have: refresh names.
    pub needs_folder_list: bool,
    /// GPU indexing waits behind a full-screen app (R-UX32).
    waiting_for_fullscreen: bool,
    /// S-04 (R40): the file reader runs with the low-integrity fallback (snapshot).
    reduced_protection: bool,
    provider_health: Option<ProviderHealthView>,
    consent_request: Option<ConsentRequest>,
    /// `sr-app --approve <id>`: the card to bring into view (cleared once decided or gone).
    focus_approval: Option<String>,
    /// [Restart now] is restarting the service (M4, page 03); cleared by the next snapshot.
    restarting: bool,
    /// "Move the search index" in progress (M4, page 32).
    index_move: Option<IndexMoveProgress>,
    /// M5 (page 33): what the organisation's policy sets (snapshot `managed`, M5-E).
    policy: PolicyView,
    /// M5 (R34): the moved index can't be found; the service waits in its recovery mode
    /// (snapshot `index_missing`); the next normal snapshot clears it.
    index_missing: Option<IndexMissingView>,
}

/// What a caller should do after applying an item.
#[derive(Debug, Default, PartialEq)]
pub struct Effects {
    pub changed: bool,
    /// Fetch `folders.list` (names/paths) once; never on a timer.
    pub refresh_folders: bool,
    /// Fetch `approvals.list` once (an `add_folder` request arrived, or a new snapshot).
    pub refresh_approvals: bool,
    /// Forward to the UI as a `ServiceSignal` event (M4: re-read one thing, or a one-off result).
    pub signal: Option<ServiceSignal>,
}

pub fn tier_name(t: Tier) -> TierName {
    match t {
        Tier::Minimum => TierName::Basic,
        Tier::Standard => TierName::Better,
        Tier::High => TierName::Best,
    }
}

fn uses_gpu(ep: &EpPlan) -> bool {
    !matches!(ep, EpPlan::Cpu)
}

impl ViewState {
    pub fn new() -> Self {
        Self::default()
    }

    /// The background service is unreachable (attach failed after retries).
    pub fn set_unavailable(&mut self, e: &Error) {
        self.connected = false;
        self.problem = Some(UiError::from(e));
    }

    pub fn set_connecting(&mut self) {
        self.connected = false;
    }

    pub fn reconnected(&mut self) {
        self.reconnects += 1;
    }

    /// [Restart now] / [Restart Selene Retrieval] (M4): the service is being restarted. While
    /// set, the shell shows "Restarting the background service…" instead of a problem panel; the
    /// next snapshot (the new run) clears it.
    pub fn set_restarting(&mut self, on: bool) -> bool {
        let changed = self.restarting != on;
        self.restarting = on;
        changed
    }

    /// Replace everything with a status snapshot (first attach, re-attach or resnapshot).
    pub fn apply_snapshot(&mut self, value: &Value) -> Effects {
        // M5 (M5-E): read from the raw snapshot before the typed parse, so the recovery-mode
        // snapshot (R34) and the policy locks show even if the rest of it doesn't parse.
        self.policy = policy::policy_view(value.get(policy::wire::MANAGED));
        self.index_missing = policy::index_missing_view(value.get(policy::wire::INDEX_MISSING));
        let Ok(s) = serde_json::from_value::<EngineStatus>(value.clone()) else {
            // Unknown shape (newer daemon): keep what we have, mark connected.
            self.connected = true;
            self.problem = None;
            self.restarting = false;
            return Effects {
                changed: true,
                refresh_folders: true,
                refresh_approvals: true,
                signal: None,
            };
        };
        self.connected = true;
        self.problem = None;
        self.restarting = false;
        self.host = Some(s.host);
        self.queued_jobs = s.queued_jobs;
        self.catch_up = s.catch_up.filter(|c| c.done < c.total);
        self.notices = s.notices;
        self.waiting_for_fullscreen = value
            .get(wire::WAITING_FOR_FULLSCREEN)
            .and_then(Value::as_bool)
            .unwrap_or(false);
        self.consent_request = value.get(wire::CONSENT_NEEDED).and_then(consent_request);
        self.index_move = s.index_move.clone();
        self.reduced_protection = s.reduced_protection;
        // M2-A R2: the tier is in the snapshot (older services: the diagnostics call fills it).
        if let Some(t) = s.tier {
            self.tier = Some(TierView {
                active: tier_name(t.active),
                recommended: tier_name(t.recommended),
                gpu: t.gpu,
                multilingual: t.multilingual,
            });
        }
        let old = std::mem::take(&mut self.folders);
        let raw = raw_folders(value);
        let mut missing_paths = false;
        for f in s.folders {
            let key = f.folder_id.to_string();
            let prev = old.get(&key);
            let extra = raw.get(&key);
            // M2-A R3: path + skipped count are in the snapshot of current services.
            let path = f.path.or_else(|| prev.and_then(|p| p.path.clone()));
            missing_paths |= path.is_none();
            self.folders.insert(
                key,
                Folder {
                    name: f.display_name,
                    path,
                    status: Some(f.status),
                    done: f.files_indexed,
                    total: f.files_total,
                    skipped: if f.files_skipped > 0 {
                        f.files_skipped
                    } else {
                        prev.map_or(0, |p| p.skipped)
                    },
                    eta_s: None,
                    scan_done: None,
                    note: extra.and_then(|e| e.note.clone()),
                    // The service's flag survives an app restart; the local mark covers the time
                    // between our `folders.remove` and the service's next snapshot.
                    removing: extra.is_some_and(|e| e.removing) || prev.is_some_and(|p| p.removing),
                    group_id: Some(f.group_id.to_string()),
                    stopped: f.stopped,
                    was_files_only: f.was_files_only,
                    online_only_skipped: extra
                        .and_then(|e| e.online_only_skipped)
                        .unwrap_or_else(|| prev.map_or(0, |p| p.online_only_skipped)),
                    cloud_note: extra.and_then(|e| e.cloud_note.clone()),
                },
            );
        }
        self.needs_folder_list = false;
        Effects {
            changed: true,
            // Only an older service leaves paths out of the snapshot.
            refresh_folders: missing_paths,
            refresh_approvals: true,
            signal: None,
        }
    }

    fn folder(&mut self, id: &FolderId) -> &mut Folder {
        let key = id.to_string();
        if !self.folders.contains_key(&key) {
            self.needs_folder_list = true;
        }
        self.folders.entry(key).or_insert_with(|| Folder {
            name: "New folder".into(),
            ..Folder::default()
        })
    }

    /// Apply one pushed event (as JSON; unknown event types from a newer daemon are ignored).
    pub fn apply_event(&mut self, payload: &Value) -> Effects {
        if let Some(changed) = self.apply_m3_event(payload) {
            return Effects {
                changed,
                ..Effects::default()
            };
        }
        let Ok(ev) = serde_json::from_value::<EngineEvent>(payload.clone()) else {
            return Effects::default();
        };
        if let Some(fx) = self.apply_m4_event(&ev) {
            return fx;
        }
        let before_unknown = self.needs_folder_list;
        let changed = match ev {
            EngineEvent::FolderStatusChanged {
                folder_id, status, ..
            } => {
                let f = self.folder(&folder_id);
                f.status = Some(status);
                if !matches!(status, FolderStatus::Scanning) {
                    f.scan_done = None;
                }
                if matches!(
                    status,
                    FolderStatus::Pending | FolderStatus::Scanning | FolderStatus::Indexing
                ) {
                    // A new run (Continue, a change, the schedule) ends "Stopped".
                    f.stopped = false;
                }
                if !matches!(status, FolderStatus::Paused | FolderStatus::Unavailable) {
                    // Amendment 17: the service clears the mark in the same transaction as the
                    // unpause, so any other status means the user answered the question.
                    f.was_files_only = false;
                }
                if matches!(
                    status,
                    FolderStatus::Synced | FolderStatus::SyncedWithErrors
                ) {
                    f.eta_s = None;
                    f.done = f.done.max(f.total);
                }
                true
            }
            EngineEvent::ScanProgress {
                folder_id, done, ..
            } => {
                let f = self.folder(&folder_id);
                f.scan_done = Some(done);
                true
            }
            EngineEvent::IndexProgress {
                folder_id,
                files_done,
                files_total,
                eta_s,
            } => {
                let f = self.folder(&folder_id);
                f.done = files_done;
                f.total = files_total;
                f.eta_s = eta_s;
                true
            }
            EngineEvent::FileFailed { folder_id, .. } => {
                self.folder(&folder_id).skipped += 1;
                true
            }
            EngineEvent::QueueDepth { queued, leased } => {
                self.queued_jobs = queued + leased;
                true
            }
            EngineEvent::PurgeCompleted { folder_id } => {
                self.folders.remove(&folder_id.to_string()).is_some()
            }
            EngineEvent::ProviderHealth { health, .. } => {
                self.provider_health = Some(match health {
                    Health::Ok => ProviderHealthView::Ok,
                    Health::Degraded => ProviderHealthView::Degraded,
                    Health::Down => ProviderHealthView::Down,
                });
                true
            }
            EngineEvent::HostState { state } => {
                self.host = Some(state);
                true
            }
            EngineEvent::CatchUpProgress { done, total } => {
                self.catch_up = (done < total).then_some(CatchUp { done, total });
                true
            }
            EngineEvent::Notice { notice } => {
                self.notices.retain(|n| n.code != notice.code);
                self.notices.push(notice);
                true
            }
            EngineEvent::ApprovalRequested { approval_id, .. } => {
                // OQ-25 (a): the window opens with this request's card on top (the push loop
                // shows it); the list refresh brings the card itself.
                self.focus_approval = Some(approval_id.to_string());
                return Effects {
                    changed: true,
                    refresh_folders: false,
                    refresh_approvals: true,
                    signal: None,
                };
            }
            EngineEvent::ModelDownloadProgress { bytes, total, .. } => {
                self.model_change = Some(model_view(
                    ModelStep::Downloading,
                    mb(bytes),
                    mb(total),
                    format!(
                        "Downloading better search… {} of {} MB",
                        mb(bytes),
                        mb(total)
                    ),
                    None,
                ));
                true
            }
            EngineEvent::ModelDownloadCompleted { .. } => {
                self.model_change = Some(model_view(
                    ModelStep::Improving,
                    0,
                    0,
                    IMPROVING.into(),
                    None,
                ));
                true
            }
            EngineEvent::ModelDownloadFailed {
                code, resumable, ..
            } => {
                let e = UiError::from(Error::new(code));
                let text = if resumable {
                    "The download stopped. It will continue where it left off.".to_owned()
                } else {
                    e.body.clone()
                };
                self.model_change = Some(model_view(ModelStep::Failed, 0, 0, text, Some(e)));
                true
            }
            EngineEvent::ReembedStarted {
                total,
                resumed_from,
                ..
            } => {
                self.model_change = Some(model_view(
                    ModelStep::Improving,
                    to_u32(resumed_from),
                    to_u32(total),
                    IMPROVING.into(),
                    None,
                ));
                true
            }
            EngineEvent::ReembedProgress { done, total, .. } => {
                let pct = (done.min(total) * 100).checked_div(total).unwrap_or(0);
                self.model_change = Some(model_view(
                    ModelStep::Improving,
                    to_u32(done),
                    to_u32(total),
                    format!("Improving search… {pct}% (search keeps working)"),
                    None,
                ));
                true
            }
            EngineEvent::ReembedFailed {
                code, resumable, ..
            } => {
                if resumable {
                    // Interrupted (shutdown, restart): it resumes by itself.
                    false
                } else {
                    let e = UiError::from(Error::new(code));
                    self.model_change =
                        Some(model_view(ModelStep::Failed, 0, 0, e.body.clone(), Some(e)));
                    true
                }
            }
            EngineEvent::IndexSwapped { model_id, .. } => {
                if let Some(t) = self.tier.as_mut() {
                    if let Some(active) = tier_of_model(&model_id) {
                        t.active = active;
                        t.multilingual = false;
                    } else if is_multilingual_model(&model_id) {
                        t.active = tier_name(Tier::Minimum);
                        t.multilingual = true;
                    }
                }
                self.model_change = Some(model_view(
                    ModelStep::Done,
                    0,
                    0,
                    "Search upgraded. Your folders now use the better search model.".into(),
                    None,
                ));
                true
            }
            EngineEvent::TierDetected {
                recommended,
                active,
                ep,
                ..
            } => {
                let multilingual = self.tier.as_ref().is_some_and(|t| t.multilingual);
                self.tier = Some(TierView {
                    active: tier_name(active),
                    recommended: tier_name(recommended),
                    gpu: uses_gpu(&ep),
                    multilingual,
                });
                true
            }
            _ => false,
        };
        Effects {
            changed,
            refresh_folders: self.needs_folder_list && !before_unknown,
            refresh_approvals: false,
            signal: None,
        }
    }

    /// M4 events (contract §3–§9). `None` = not one of them.
    fn apply_m4_event(&mut self, ev: &EngineEvent) -> Option<Effects> {
        let signal = |s: ServiceSignal| Effects {
            signal: Some(s),
            ..Effects::default()
        };
        Some(match ev {
            EngineEvent::GroupsChanged => signal(ServiceSignal::GroupsChanged),
            EngineEvent::CliSigninChanged(st) => signal(ServiceSignal::CliSigninChanged {
                status: crate::cli_signin::status_from(
                    &st.provider,
                    &serde_json::to_value(st).unwrap_or_default(),
                ),
            }),
            EngineEvent::ClientsChanged { .. } | EngineEvent::HttpState(_) => {
                signal(ServiceSignal::ClientsChanged)
            }
            EngineEvent::ActivityAdded { .. } => signal(ServiceSignal::ActivityAdded),
            EngineEvent::EgressRecorded { .. } => signal(ServiceSignal::EgressRecorded),
            EngineEvent::RequestProgress(p) => signal(ServiceSignal::RequestProgress {
                request_id: p.request_id.clone(),
                line: request_line(p),
            }),
            EngineEvent::IndexMoveProgress(p) => {
                let changed = self.index_move.as_ref() != Some(p);
                self.index_move = Some(p.clone());
                Effects {
                    changed,
                    ..Effects::default()
                }
            }
            EngineEvent::IndexMoveDone { path } => {
                self.index_move = None;
                Effects {
                    changed: true,
                    signal: Some(ServiceSignal::IndexMoveDone { path: path.clone() }),
                    ..Effects::default()
                }
            }
            EngineEvent::IndexMoveFailed { code, kept_path } => {
                self.index_move = None;
                Effects {
                    changed: true,
                    signal: Some(ServiceSignal::IndexMoveFailed {
                        error: UiError::from(Error::new(*code)),
                        kept_path: kept_path.clone(),
                    }),
                    ..Effects::default()
                }
            }
            _ => return None,
        })
    }

    /// M3 events not yet typed in `sr_common` (see [`wire`]). `None` = not one of them.
    fn apply_m3_event(&mut self, payload: &Value) -> Option<bool> {
        match payload.get("type").and_then(Value::as_str)? {
            wire::EV_FULLSCREEN_WAIT => {
                let w = payload
                    .get("waiting")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let changed = w != self.waiting_for_fullscreen;
                self.waiting_for_fullscreen = w;
                Some(changed)
            }
            wire::EV_CONSENT_NEEDED => {
                self.consent_request = consent_request(payload);
                Some(true)
            }
            wire::EV_CONSENT_CHANGED => {
                let purpose = payload
                    .get("purpose")
                    .and_then(Value::as_str)
                    .and_then(ConsentPurpose::from_wire);
                let answered = self
                    .consent_request
                    .as_ref()
                    .is_some_and(|r| Some(r.purpose) == purpose);
                if answered {
                    self.consent_request = None;
                }
                Some(answered)
            }
            _ => None,
        }
    }

    /// The consent dialog for the pending request was answered here (Allow or Don't allow).
    pub fn clear_consent_request(&mut self) -> bool {
        self.consent_request.take().is_some()
    }

    /// `approvals.list` reply (pending `add_folder` requests). Duplicate requests for the same
    /// client and folder show one card (page 14).
    pub fn apply_approvals(&mut self, list: &Value) -> bool {
        let Some(arr) = list.as_array() else {
            return false;
        };
        let mut next: Vec<ApprovalView> = Vec::new();
        for a in arr {
            let Some(v) = approval_view(a) else { continue };
            let dup = next
                .iter()
                .any(|n| n.client == v.client && n.path.eq_ignore_ascii_case(&v.path));
            if !dup {
                next.push(v);
            }
        }
        if let Some(id) = &self.focus_approval
            && !next.iter().any(|a| &a.id == id)
        {
            // Decided elsewhere or expired: nothing to bring into view any more.
            self.focus_approval = None;
        }
        let changed = next != self.approvals;
        self.approvals = next;
        changed
    }

    /// The user decided a request here: drop it at once (the list refresh confirms it).
    pub fn remove_approval(&mut self, id: &str) -> bool {
        let n = self.approvals.len();
        self.approvals.retain(|a| a.id != id);
        if self.focus_approval.as_deref() == Some(id) {
            self.focus_approval = None;
        }
        n != self.approvals.len()
    }

    /// `sr-app --approve <id>` (R-UX30): bring that request's card into view.
    pub fn set_focus_approval(&mut self, id: String) -> bool {
        let changed = self.focus_approval.as_deref() != Some(id.as_str());
        self.focus_approval = Some(id);
        changed
    }

    /// `folders.remove` succeeded: the card shows "Removing…" until `PurgeCompleted` (OQ-18).
    pub fn mark_removing(&mut self, folder_id: &str) -> bool {
        match self.folders.get_mut(folder_id) {
            Some(f) if !f.removing => {
                f.removing = true;
                true
            }
            _ => false,
        }
    }

    /// [Stop] (`folders.cancel`) succeeded: "Stopped" until the next run starts (OQ-37, OQ-39).
    pub fn mark_stopped(&mut self, folder_id: &str) -> bool {
        match self.folders.get_mut(folder_id) {
            Some(f) if !f.stopped => {
                f.stopped = true;
                true
            }
            _ => false,
        }
    }

    /// `notices.dismiss` succeeded: hide them until the next start.
    pub fn clear_notices(&mut self) -> bool {
        let had = !self.notices.is_empty();
        self.notices.clear();
        had
    }

    /// `search_model.status` (once per attach): a change already in progress shows its step.
    pub fn apply_search_model_status(&mut self, st: &Value) -> bool {
        let phase = &st["phase"];
        let n = |k: &str| phase.get(k).and_then(Value::as_u64).unwrap_or(0);
        let v = match phase.get("phase").and_then(Value::as_str) {
            Some("downloading") => model_view(
                ModelStep::Downloading,
                mb(n("bytes")),
                mb(n("total")),
                format!(
                    "Downloading better search… {} of {} MB",
                    mb(n("bytes")),
                    mb(n("total"))
                ),
                None,
            ),
            Some("waiting_for_unmetered_network") => model_view(
                ModelStep::WaitingForUnmetered,
                mb(n("have")),
                mb(n("total")),
                "Paused because this connection may cost money (metered).".into(),
                None,
            ),
            Some("rebuilding") => model_view(
                ModelStep::Improving,
                to_u32(n("done")),
                to_u32(n("total")),
                IMPROVING.into(),
                None,
            ),
            _ => return false,
        };
        self.model_change = Some(v);
        true
    }

    /// [Use it] was clicked: show the first step at once (events take over).
    pub fn model_change_started(&mut self) {
        self.model_change = Some(model_view(
            ModelStep::Downloading,
            0,
            0,
            "Starting the download…".into(),
            None,
        ));
    }

    pub fn model_change_cancelled(&mut self) {
        self.model_change = None;
    }

    /// Merge `folders.list` (names, paths, skipped counts). Status/progress stay event-driven.
    pub fn apply_folder_list(&mut self, list: &Value) -> bool {
        self.merge_folders(list, true)
    }

    /// Merge one `FolderInfo` (the `folders.add` reply), so a new folder shows its real name and
    /// path at once instead of waiting for the first event + list refresh.
    pub fn apply_folder_info(&mut self, info: &Value) -> bool {
        self.merge_folders(&Value::Array(vec![info.clone()]), false)
    }

    fn merge_folders(&mut self, list: &Value, prune: bool) -> bool {
        let Some(arr) = list.as_array() else {
            return false;
        };
        let mut seen = Vec::new();
        for f in arr {
            let Some(id) = f.get("folder_id").and_then(Value::as_str) else {
                continue;
            };
            seen.push(id.to_owned());
            let e = self.folders.entry(id.to_owned()).or_default();
            if let Some(n) = f.get("name").and_then(Value::as_str) {
                e.name = n.to_owned();
            }
            e.path = f.get("path").and_then(Value::as_str).map(str::to_owned);
            if let Some(n) = f.get("files_skipped").and_then(Value::as_u64) {
                e.skipped = n;
            }
            if let Some(n) = f.get(wire::WATCH_NOTE) {
                e.note = n.as_str().map(str::to_owned).filter(|s| !s.is_empty());
            }
            if f.get(wire::REMOVING).and_then(Value::as_bool) == Some(true) {
                e.removing = true;
            }
            if let Some(g) = f.get("group_id").and_then(Value::as_str) {
                e.group_id = Some(g.to_owned());
            }
            e.stopped = f.get("stopped").and_then(Value::as_bool).unwrap_or(false);
            e.was_files_only = f
                .get("was_files_only")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let (online_only, cloud_note) = policy::cloud_fields(f);
            if let Some(n) = online_only {
                e.online_only_skipped = n;
            }
            if cloud_note.is_some() {
                e.cloud_note = cloud_note;
            }
            if e.status.is_none() {
                e.status = f
                    .get("status")
                    .and_then(|s| serde_json::from_value(s.clone()).ok());
                e.done = f.get("files_indexed").and_then(Value::as_u64).unwrap_or(0);
                e.total = f.get("files_total").and_then(Value::as_u64).unwrap_or(0);
            }
        }
        if prune {
            // A folder being removed stays until `PurgeCompleted` (or the next snapshot).
            self.folders.retain(|k, f| f.removing || seen.contains(k));
            self.needs_folder_list = false;
        }
        true
    }

    /// Tier from `engine.diagnostics` (one call per attach; `tier.detected` is only pushed at
    /// daemon start, so an app attaching later would otherwise miss it).
    pub fn apply_tier_decision(&mut self, decision: &Value) {
        if let Ok(d) = serde_json::from_value::<TierDecision>(decision.clone()) {
            let gpu = uses_gpu(&d.ep);
            let active = self
                .tier
                .as_ref()
                .map_or(tier_name(Tier::Minimum), |t| t.active);
            let multilingual = self.tier.as_ref().is_some_and(|t| t.multilingual);
            self.tier = Some(TierView {
                active,
                recommended: tier_name(d.tier),
                gpu,
                multilingual,
            });
        }
    }

    pub fn tier(&self) -> Option<&TierView> {
        self.tier.as_ref()
    }

    pub fn view(&self) -> AppView {
        let folders: Vec<FolderView> = self
            .folders
            .iter()
            .map(|(id, f)| folder_view(id, f, self.waiting_for_fullscreen))
            .collect();
        let status = EngineStatus {
            host: self.host.unwrap_or(HostState::Starting),
            folders: self
                .folders
                .iter()
                .filter(|(_, f)| !f.removing)
                .filter_map(|(id, f)| {
                    Some(FolderStatusSummary {
                        folder_id: id.parse().ok()?,
                        display_name: f.name.clone(),
                        status: f.status.unwrap_or(FolderStatus::Pending),
                        files_indexed: f.done,
                        files_total: f.total,
                        path: None,
                        files_skipped: 0,
                        ..FolderStatusSummary::default()
                    })
                })
                .collect(),
            queued_jobs: self.queued_jobs,
            summary: String::new(),
            catch_up: self.catch_up,
            notices: Vec::new(),
            tier: None,
            ..EngineStatus::default()
        };
        let overall = if self.restarting || self.index_move.is_some() {
            Overall::Starting
        } else if self.index_missing.is_some() || (!self.connected && self.problem.is_some()) {
            Overall::Problem
        } else if !self.connected || self.host.is_none_or(|h| h == HostState::Starting) {
            Overall::Starting
        } else if folders
            .iter()
            .any(|f| matches!(f.state, FolderState::Unavailable | FolderState::Problem))
        {
            Overall::Problem
        } else if self.catch_up.is_some() {
            Overall::Indexing
        } else if folders.is_empty() {
            Overall::NoFolders
        } else if folders.iter().any(|f| {
            matches!(
                f.state,
                FolderState::Waiting | FolderState::Scanning | FolderState::Indexing
            )
        }) {
            Overall::Indexing
        } else {
            Overall::Synced
        };
        let summary = match overall {
            Overall::Problem if self.index_missing.is_some() => self
                .index_missing
                .as_ref()
                .map_or_else(String::new, |m| m.error.title.clone()),
            Overall::Problem if !self.connected => self
                .problem
                .as_ref()
                .map_or_else(|| "Something went wrong".into(), |p| p.title.clone()),
            Overall::Problem => "A folder needs your attention".into(),
            Overall::Starting if self.index_move.is_some() => format!(
                "Moving the search index ({}%)",
                self.index_move
                    .as_ref()
                    .map_or(0, IndexMoveProgress::percent)
            ),
            Overall::Starting => "Starting…".into(),
            Overall::Indexing if self.catch_up.is_some() => status.plain_summary(),
            Overall::Indexing if self.waiting_for_fullscreen => FULLSCREEN_WAIT.into(),
            Overall::Indexing => {
                // plain_summary counts done < total; also cover scanning (total unknown yet).
                let s = status.plain_summary();
                if s.starts_with("Indexing") {
                    format_counts(&s)
                } else {
                    "Looking through your files…".into()
                }
            }
            Overall::NoFolders => "No folders yet".into(),
            Overall::Synced => "Up to date".into(),
        };
        AppView {
            overall,
            summary,
            folders,
            // R34 has its own screen instead of the generic "didn't start" panel.
            problem: if self.connected || self.restarting || self.index_missing.is_some() {
                None
            } else {
                self.problem.clone()
            },
            tier: self.tier.clone(),
            reconnects: self.reconnects,
            notices: self
                .notices
                .iter()
                .map(|n| UiError::from(&n.message))
                .collect(),
            model_change: self.model_change.clone(),
            approvals: self.approvals.clone(),
            catching_up: self.connected && self.catch_up.is_some(),
            waiting_for_fullscreen: self.connected && self.waiting_for_fullscreen,
            provider_health: self.provider_health,
            consent_request: self.consent_request.clone(),
            focus_approval: self.focus_approval.clone(),
            restarting: self.restarting,
            index_move: self.index_move.as_ref().map(index_move_view),
            policy: self.policy.clone(),
            index_missing: self.index_missing.clone(),
            reduced_protection: self.connected && self.reduced_protection,
        }
    }

    /// The R34 choice was sent ([Move the index back] / [Start fresh]): the screen gives way to
    /// "Restarting the background service..." until the new run's snapshot arrives.
    pub fn clear_index_missing(&mut self) -> bool {
        self.index_missing.take().is_some()
    }
}

/// Fields of the raw snapshot's folder rows read from the JSON (forward-compatible).
struct RawFolder {
    note: Option<String>,
    removing: bool,
    online_only_skipped: Option<u64>,
    cloud_note: Option<String>,
}

/// Plain phase line of an Ask / Prompt Enhancer request (pages 27, 28).
pub fn request_line(p: &RequestProgress) -> String {
    match (p.kind, p.phase) {
        (RequestKind::Enhance, _) => "Improving your prompt…".into(),
        (RequestKind::Answer, RequestPhase::Writing) => "Writing the answer…".into(),
        (RequestKind::Answer, RequestPhase::SearchingAgain) => match (p.round, p.max) {
            (Some(n), Some(m)) => format!("Searching again (round {n} of {m})…"),
            _ => "Searching again…".into(),
        },
        (RequestKind::Answer, _) => "Reading your files…".into(),
    }
}

/// GB with one decimal ("1.4").
fn gb(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss)] // display only
    let g = bytes as f64 / 1_000_000_000.0;
    format!("{g:.1}")
}

fn index_move_view(p: &IndexMoveProgress) -> IndexMoveView {
    let line = match p.phase {
        MovePhase::Waiting => "Starting Selene Retrieval…".to_owned(),
        MovePhase::Copying | MovePhase::Verifying | MovePhase::Switching | MovePhase::Cleaning => {
            format!("Moving… {} of {} GB", gb(p.done_bytes), gb(p.total_bytes))
        }
    };
    IndexMoveView {
        can_cancel: matches!(
            p.phase,
            MovePhase::Waiting | MovePhase::Copying | MovePhase::Verifying
        ),
        percent: u32::try_from(p.percent().min(100)).unwrap_or(100),
        line,
        from: p.from.clone(),
        to: p.to.clone(),
    }
}

fn raw_folders(snapshot: &Value) -> BTreeMap<String, RawFolder> {
    snapshot
        .get("folders")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|f| {
            let id = f.get("folder_id")?.as_str()?.to_owned();
            let note = f
                .get(wire::WATCH_NOTE)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .filter(|s| !s.is_empty());
            let removing = f
                .get(wire::REMOVING)
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let (online_only_skipped, cloud_note) = policy::cloud_fields(f);
            Some((
                id,
                RawFolder {
                    note,
                    removing,
                    online_only_skipped,
                    cloud_note,
                },
            ))
        })
        .collect()
}

/// `{"provider": "<id>", "purpose": "answer"}` → the dialog request (unknown purposes ignored).
fn consent_request(v: &Value) -> Option<ConsentRequest> {
    let provider = v.get("provider")?.as_str()?.to_owned();
    let purpose = ConsentPurpose::from_wire(v.get("purpose")?.as_str()?)?;
    Some(ConsentRequest {
        provider_name: crate::providers::display_name(&provider),
        provider,
        purpose,
    })
}

fn approval_view(a: &Value) -> Option<ApprovalView> {
    #[allow(clippy::cast_precision_loss)] // ms timestamps stay far below 2^53
    let expires_at_ms = a.get("expires_at").and_then(Value::as_i64).unwrap_or(0) as f64;
    Some(ApprovalView {
        id: a.get("approval_id")?.as_str()?.to_owned(),
        client: client_word(a.get("client").and_then(Value::as_str)),
        path: a.get("path")?.as_str()?.to_owned(),
        expires_at_ms,
    })
}

const IMPROVING: &str = "Improving search… (search keeps working)";

fn model_view(
    step: ModelStep,
    done: u32,
    total: u32,
    text: String,
    error: Option<UiError>,
) -> ModelChangeView {
    ModelChangeView {
        step,
        done,
        total,
        text,
        error,
    }
}

fn mb(bytes: u64) -> u32 {
    to_u32(bytes.div_ceil(1_000_000))
}

/// The pinned tier models (sr_common::tier) by id.
fn tier_of_model(model_id: &str) -> Option<TierName> {
    [Tier::Minimum, Tier::Standard, Tier::High]
        .into_iter()
        .find(|t| sr_common::tier::TierProfile::for_tier(*t).model_id == model_id)
        .map(tier_name)
}

/// The multilingual search model (`intfloat/multilingual-e5-small`, M3-Q), which isn't a tier.
fn is_multilingual_model(model_id: &str) -> bool {
    model_id.to_ascii_lowercase().contains("multilingual-e5")
}

fn client_word(kind: Option<&str>) -> String {
    match kind {
        Some("claude_desktop") => "Claude",
        Some("cursor") => "Cursor",
        Some("vs_code" | "vscode") => "VS Code",
        _ => "An app",
    }
    .into()
}

/// "Indexing 340 of 2100 files" → "Indexing 340 of 2,100 files".
fn format_counts(s: &str) -> String {
    s.split(' ')
        .map(|w| w.parse::<u64>().map_or_else(|_| w.to_owned(), thousands))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// "a minute or two", "about 4 minutes", "about 2 hours". Short times are deliberately vague: the
/// first pass has fixed costs (model load, workers) that dominate small folders.
pub fn duration_text(secs: u64) -> String {
    match secs {
        0..=119 => "a minute or two".into(),
        120..=3_599 => format!("about {} minutes", secs.div_ceil(60)),
        3_600..=7_199 => "about an hour".into(),
        _ => format!("about {} hours", secs.div_ceil(3_600)),
    }
}

fn to_u32(n: u64) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn folder_view(id: &str, f: &Folder, waiting_for_fullscreen: bool) -> FolderView {
    let state = match f.status.unwrap_or(FolderStatus::Pending) {
        _ if f.removing => FolderState::Removing,
        // OQ-37: the user stopped the run; until a full check finishes, it isn't "up to date".
        FolderStatus::Synced | FolderStatus::SyncedWithErrors | FolderStatus::Partial
            if f.stopped =>
        {
            FolderState::Stopped
        }
        FolderStatus::Pending => FolderState::Waiting,
        FolderStatus::Scanning => FolderState::Scanning,
        FolderStatus::Indexing => FolderState::Indexing,
        FolderStatus::Synced if f.skipped > 0 => FolderState::UpToDateWithSkips,
        FolderStatus::Synced => FolderState::UpToDate,
        FolderStatus::SyncedWithErrors => FolderState::UpToDateWithSkips,
        // A run that stopped before the end is not "up to date" (OQ-37, X3).
        FolderStatus::Partial => FolderState::Stopped,
        FolderStatus::Paused => FolderState::Paused,
        FolderStatus::Unavailable => FolderState::Unavailable,
        FolderStatus::Error => FolderState::Problem,
    };
    let line = match state {
        FolderState::Waiting => "Waiting to start".into(),
        FolderState::Scanning => match f.scan_done {
            Some(n) if n > 0 => format!("Looking through files… {} found", thousands(n)),
            _ => "Looking through files…".into(),
        },
        FolderState::Indexing => {
            let mut s = if f.total > 0 {
                format!(
                    "Indexing {} of {} files",
                    thousands(f.done.min(f.total)),
                    thousands(f.total)
                )
            } else {
                "Indexing…".into()
            };
            if waiting_for_fullscreen {
                // The ETA doesn't move while indexing waits; say why instead (R-UX32).
                s.push_str(" · ");
                s.push_str(FULLSCREEN_WAIT);
            } else if let Some(eta) = f.eta_s {
                s.push_str(&format!(" · {} left", duration_text(eta)));
            }
            s
        }
        FolderState::UpToDate => format!("Up to date · {} files", thousands(f.total)),
        FolderState::UpToDateWithSkips => format!(
            "Up to date · {} files ({} skipped)",
            thousands(f.total.saturating_sub(f.skipped)),
            thousands(f.skipped)
        ),
        FolderState::Paused if f.was_files_only => WAS_FILES_ONLY_LINE.into(),
        FolderState::Paused => "Paused".into(),
        FolderState::Unavailable => {
            "Can't reach this folder. The drive may be unplugged or the folder moved.".into()
        }
        FolderState::Problem => "Something went wrong with this folder.".into(),
        FolderState::Removing => "Removing… It no longer appears in search.".into(),
        FolderState::Stopped => format!(
            "Stopped · {} of {} files. Search covers what's done.",
            thousands(f.done.min(f.total)),
            thousands(f.total)
        ),
    };
    FolderView {
        id: id.to_owned(),
        name: f.name.clone(),
        path: f.path.clone(),
        state,
        line,
        files_done: to_u32(f.done),
        files_total: to_u32(f.total),
        files_skipped: to_u32(f.skipped),
        eta_s: f.eta_s.map(to_u32),
        // The note is a modifier (DS-5.3); a folder being removed shows only "Removing…".
        note: f
            .note
            .clone()
            .filter(|_| state != FolderState::Removing)
            .map(|text| FolderNote {
                kind: FolderNoteKind::Network,
                text,
            }),
        group_id: f.group_id.clone(),
        was_files_only: f.was_files_only && state != FolderState::Removing,
        online_only_skipped: to_u32(f.online_only_skipped),
        cloud_note: f
            .cloud_note
            .clone()
            .filter(|_| state != FolderState::Removing),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use sr_common::error::ErrorCode;

    fn snap(folders: Value) -> Value {
        json!({"host": "ready", "folders": folders, "queued_jobs": 0, "summary": ""})
    }

    #[test]
    fn m4_events_become_signals_and_move_progress() {
        let mut s = ViewState::new();
        s.apply_snapshot(&snap(json!([])));
        for (ev, want) in [
            (
                json!({"type": "groups.changed"}),
                ServiceSignal::GroupsChanged,
            ),
            (
                json!({"type": "clients.changed", "client_id": "cursor"}),
                ServiceSignal::ClientsChanged,
            ),
            (
                json!({"type": "http.state", "enabled": true, "running": true, "port": 1,
                       "url": "http://127.0.0.1:1/mcp", "error": null, "tools": 0}),
                ServiceSignal::ClientsChanged,
            ),
        ] {
            let fx = s.apply_event(&ev);
            assert_eq!(fx.signal, Some(want), "{ev}");
            assert!(!fx.changed, "signals don't touch the view");
        }
        let fx = s.apply_event(&json!({"type": "request.progress", "request_id": "r1",
            "kind": "answer", "phase": "searching_again", "round": 1, "max": 2}));
        assert_eq!(
            fx.signal,
            Some(ServiceSignal::RequestProgress {
                request_id: "r1".into(),
                line: "Searching again (round 1 of 2)…".into()
            })
        );
        // Move progress: the shell says so; the dialog gets the line.
        let fx = s.apply_event(&json!({"type": "index.move.progress", "phase": "copying",
            "done_bytes": 1_100_000_000_u64, "total_bytes": 2_400_000_000_u64,
            "from": "C:/a", "to": "D:/b"}));
        assert!(fx.changed);
        let v = s.view();
        assert_eq!(v.overall, Overall::Starting);
        assert!(
            v.summary.starts_with("Moving the search index ("),
            "{}",
            v.summary
        );
        let m = v.index_move.unwrap();
        assert_eq!(m.line, "Moving… 1.1 of 2.4 GB");
        assert!(m.can_cancel);
        let fx = s.apply_event(
            &json!({"type": "index.move.failed", "code": "DiskFull", "kept_path": "C:/a"}),
        );
        assert!(matches!(
            fx.signal,
            Some(ServiceSignal::IndexMoveFailed { .. })
        ));
        assert!(s.view().index_move.is_none());
    }

    #[test]
    fn stop_shows_stopped_until_the_next_run() {
        let mut s = ViewState::new();
        let id = sr_common::ids::FolderId::new().to_string();
        s.apply_snapshot(&snap(json!([{"folder_id": id, "display_name": "Docs",
            "status": "indexing", "files_indexed": 4, "files_total": 10}])));
        assert!(s.mark_stopped(&id));
        s.apply_event(&json!({"type": "folder_status_changed", "folder_id": id,
            "status": "synced", "reason": null}));
        assert_eq!(s.view().folders[0].state, FolderState::Stopped);
        // [Continue] starts a new run: no longer stopped.
        s.apply_event(&json!({"type": "folder_status_changed", "folder_id": id,
            "status": "scanning", "reason": null}));
        assert_eq!(s.view().folders[0].state, FolderState::Scanning);
        // The service's own flag (snapshot) also counts.
        s.apply_snapshot(&snap(json!([{"folder_id": id, "display_name": "Docs",
            "status": "synced", "files_indexed": 4, "files_total": 10, "stopped": true}])));
        let f = &s.view().folders[0];
        assert_eq!(f.state, FolderState::Stopped);
    }

    /// Amendment 17: a folder paused because it was set to only list its files says so on its
    /// line (snapshot and `folders.list` alike) until the user chooses; then it is an ordinary
    /// folder again at once, from the pushed status alone (no snapshot or list follows a resume).
    #[test]
    fn a_former_files_only_folder_asks_what_to_do() {
        let mut s = ViewState::new();
        let id = sr_common::ids::FolderId::new().to_string();
        let status = |st: &str| json!({"type": "folder_status_changed", "folder_id": id, "status": st, "reason": null});
        s.apply_snapshot(&snap(json!([{"folder_id": id, "display_name": "Scans",
            "status": "paused", "files_indexed": 0, "files_total": 3, "was_files_only": true}])));
        let f = &s.view().folders[0];
        assert_eq!(f.state, FolderState::Paused);
        assert!(f.was_files_only);
        assert_eq!(f.line, WAS_FILES_ONLY_LINE);
        s.apply_folder_list(
            &json!([{"folder_id": id, "name": "Scans", "path": "C:\\Scans",
            "was_files_only": true}]),
        );
        assert!(s.view().folders[0].was_files_only, "the list keeps it");
        // The drive goes away and comes back: the question is still open.
        for st in ["unavailable", "paused"] {
            s.apply_event(&status(st));
            assert!(s.view().folders[0].was_files_only, "{st}");
        }
        assert_eq!(s.view().folders[0].line, WAS_FILES_ONLY_LINE);
        // [Search its contents]: the service unpauses (clearing the mark) and scans.
        s.apply_event(&status("scanning"));
        let f = &s.view().folders[0];
        assert!(!f.was_files_only, "answered");
        assert_eq!(f.line, "Looking through files…");
        // Paused again later: an ordinary pause, and the next list agrees.
        s.apply_event(&status("paused"));
        assert_eq!(s.view().folders[0].line, "Paused");
        s.apply_folder_list(&json!([{"folder_id": id, "name": "Scans", "path": "C:\\Scans"}]));
        let f = &s.view().folders[0];
        assert!(!f.was_files_only);
        assert_eq!(f.line, "Paused");
    }

    #[test]
    fn restarting_hides_the_problem_until_the_new_run_attaches() {
        let mut s = ViewState::new();
        s.apply_snapshot(&snap(json!([])));
        assert!(s.set_restarting(true));
        assert!(!s.set_restarting(true), "no change, no push");
        // The drain drops the link: no "didn't start" panel while restarting.
        s.set_unavailable(&Error::new(ErrorCode::ServiceUnavailable));
        let v = s.view();
        assert!(v.restarting);
        assert_eq!(v.overall, Overall::Starting);
        assert_eq!(v.summary, "Starting…");
        assert!(v.problem.is_none());
        // The new run's snapshot ends it.
        s.apply_snapshot(&snap(json!([])));
        let v = s.view();
        assert!(!v.restarting);
        assert_eq!(v.overall, Overall::NoFolders);
        // A restart that never came back shows the real problem once the flag is dropped.
        s.set_restarting(true);
        s.set_unavailable(&Error::new(ErrorCode::ServiceUnavailable));
        s.set_restarting(false);
        assert_eq!(s.view().overall, Overall::Problem);
        assert!(s.view().problem.is_some());
    }

    #[test]
    fn starts_in_starting_then_no_folders() {
        let mut s = ViewState::new();
        assert_eq!(s.view().overall, Overall::Starting);
        let fx = s.apply_snapshot(&snap(json!([])));
        assert!(fx.changed && !fx.refresh_folders);
        let v = s.view();
        assert_eq!(v.overall, Overall::NoFolders);
        assert_eq!(v.summary, "No folders yet");
    }

    #[test]
    fn progress_events_drive_the_view() {
        let id = FolderId::new();
        let mut s = ViewState::new();
        s.apply_snapshot(&snap(json!([{
            "folder_id": id, "display_name": "Docs", "status": "pending",
            "files_indexed": 0, "files_total": 0
        }])));
        assert_eq!(s.view().overall, Overall::Indexing);
        s.apply_event(&json!({"type": "folder_status_changed", "folder_id": id, "status": "indexing", "reason": null}));
        s.apply_event(&json!({"type": "index_progress", "folder_id": id, "files_done": 340, "files_total": 2100, "eta_s": 240}));
        let v = s.view();
        assert_eq!(v.overall, Overall::Indexing);
        assert_eq!(v.summary, "Indexing 340 of 2,100 files");
        assert_eq!(
            v.folders[0].line,
            "Indexing 340 of 2,100 files · about 4 minutes left"
        );
        s.apply_event(&json!({"type": "folder_status_changed", "folder_id": id, "status": "synced", "reason": null}));
        let v = s.view();
        assert_eq!(v.overall, Overall::Synced);
        assert_eq!(v.folders[0].line, "Up to date · 2,100 files");
        assert_eq!(v.folders[0].eta_s, None);
    }

    #[test]
    fn unknown_folder_requests_one_list_refresh() {
        let mut s = ViewState::new();
        s.apply_snapshot(&snap(json!([])));
        let id = FolderId::new();
        let fx = s.apply_event(
            &json!({"type": "scan_progress", "folder_id": id, "done": 5, "total_est": null}),
        );
        assert!(fx.refresh_folders);
        let fx = s.apply_event(
            &json!({"type": "scan_progress", "folder_id": id, "done": 9, "total_est": null}),
        );
        assert!(!fx.refresh_folders, "only once until the list arrives");
        assert!(s.apply_folder_list(&json!([{
            "folder_id": id, "name": "Notes", "path": "C:\\Users\\a\\Notes", "status": "scanning",
            "available": true, "files_indexed": 0, "files_total": 0, "files_skipped": 2, "last_success_at": null
        }])));
        let v = s.view();
        assert_eq!(v.folders[0].name, "Notes");
        assert_eq!(v.folders[0].path.as_deref(), Some("C:\\Users\\a\\Notes"));
        assert_eq!(v.folders[0].line, "Looking through files… 9 found");
    }

    #[test]
    fn add_reply_names_the_folder_at_once() {
        let mut s = ViewState::new();
        s.apply_snapshot(&snap(json!([])));
        let id = FolderId::new();
        assert!(s.apply_folder_info(&json!({
            "folder_id": id, "name": "Docs", "path": r"C:\Docs", "status": "pending",
            "available": true, "files_indexed": 0, "files_total": 0, "files_skipped": 0, "last_success_at": null
        })));
        let fx = s.apply_event(&json!({"type": "index_progress", "folder_id": id, "files_done": 1, "files_total": 4, "eta_s": null}));
        assert!(!fx.refresh_folders, "known folder: no list refresh");
        let v = s.view();
        assert_eq!(
            (v.folders[0].name.as_str(), v.folders[0].path.as_deref()),
            ("Docs", Some(r"C:\Docs"))
        );
        assert_eq!(v.overall, Overall::Indexing);
    }

    #[test]
    fn unavailable_folder_and_service_down_are_problems() {
        let id = FolderId::new();
        let mut s = ViewState::new();
        s.apply_snapshot(&snap(json!([{
            "folder_id": id, "display_name": "USB", "status": "synced", "files_indexed": 3, "files_total": 3
        }])));
        s.apply_event(&json!({"type": "folder_status_changed", "folder_id": id, "status": "unavailable", "reason": "FolderUnavailable"}));
        assert_eq!(s.view().overall, Overall::Problem);
        s.set_unavailable(&Error::new(ErrorCode::ServiceUnavailable));
        let v = s.view();
        assert_eq!(v.overall, Overall::Problem);
        assert_eq!(v.summary, "Background service didn't start");
        assert!(v.problem.is_some());
        // Re-attach: snapshot clears the problem and counts the reconnect.
        s.reconnected();
        s.apply_snapshot(&snap(json!([])));
        let v = s.view();
        assert!(v.problem.is_none());
        assert_eq!(v.reconnects, 1);
    }

    #[test]
    fn purge_removes_and_unknown_events_are_ignored() {
        let id = FolderId::new();
        let mut s = ViewState::new();
        s.apply_snapshot(&snap(json!([{
            "folder_id": id, "display_name": "A", "status": "synced", "files_indexed": 1, "files_total": 1
        }])));
        assert!(
            !s.apply_event(&json!({"type": "something_from_the_future"}))
                .changed
        );
        assert!(
            s.apply_event(&json!({"type": "purge_completed", "folder_id": id}))
                .changed
        );
        assert_eq!(s.view().overall, Overall::NoFolders);
    }

    #[test]
    fn tier_from_event_and_diagnostics() {
        let mut s = ViewState::new();
        s.apply_tier_decision(&json!({"tier": "standard", "ep": {"kind": "direct_ml", "dxgi_index": 0}, "gpu_class": "dedicated", "reasons": []}));
        let t = s.tier().cloned().unwrap();
        assert_eq!(
            (t.active, t.recommended, t.gpu),
            (TierName::Basic, TierName::Better, true)
        );
        s.apply_event(&json!({"type": "tier.detected", "recommended": "high", "active": "minimum", "override_": null,
            "ep": {"kind": "cpu"}, "gpu_class": "none", "reasons": ["x"]}));
        let t = s.tier().cloned().unwrap();
        assert_eq!((t.recommended, t.gpu), (TierName::Best, false));
    }

    #[test]
    fn catch_up_and_notices_reach_the_view() {
        let mut s = ViewState::new();
        s.apply_snapshot(&json!({"host": "ready", "folders": [], "queued_jobs": 0, "summary": "",
            "catch_up": {"done": 10, "total": 40},
            "notices": [{"code": "SettingsInvalid", "message": Error::new(ErrorCode::SettingsInvalid).user_message()}]}));
        let v = s.view();
        assert_eq!(v.overall, Overall::Indexing);
        assert_eq!(v.summary, "Catching up after restart (25%)");
        assert_eq!(v.notices.len(), 1);
        assert_eq!(v.notices[0].title, "Some settings were reset");
        assert_eq!(v.notices[0].action, crate::dto::UiAction::OpenSettings);
        s.apply_event(&json!({"type": "catch_up_progress", "done": 40, "total": 40}));
        assert_eq!(s.view().overall, Overall::NoFolders);
        // Same notice again replaces, never duplicates.
        let n = json!({"code": "SettingsInvalid", "message": Error::new(ErrorCode::SettingsInvalid).user_message()});
        s.apply_event(&json!({"type": "notice", "notice": n}));
        assert_eq!(s.view().notices.len(), 1);
    }

    #[test]
    fn removing_stays_until_purge_and_partial_is_stopped() {
        let (a, b) = (FolderId::new(), FolderId::new());
        let mut s = ViewState::new();
        s.apply_snapshot(&snap(json!([
            {"folder_id": a, "display_name": "A", "status": "synced", "files_indexed": 2, "files_total": 2},
            {"folder_id": b, "display_name": "B", "status": "partial", "files_indexed": 40, "files_total": 2100,
             "watch_note": "This folder is on a network drive, so changes are picked up every hour."}
        ])));
        let v = s.view();
        let fb = v.folders.iter().find(|f| f.name == "B").unwrap();
        assert_eq!(fb.state, FolderState::Stopped);
        assert_eq!(
            fb.line,
            "Stopped · 40 of 2,100 files. Search covers what's done."
        );
        assert_eq!(
            fb.note.as_ref().map(|n| n.text.as_str()),
            Some("This folder is on a network drive, so changes are picked up every hour.")
        );
        assert!(s.mark_removing(&a.to_string()));
        let v = s.view();
        let fa = v.folders.iter().find(|f| f.name == "A").unwrap();
        assert_eq!(fa.state, FolderState::Removing);
        assert_eq!(fa.line, "Removing… It no longer appears in search.");
        // A list refresh that no longer has it keeps the card until the purge.
        s.apply_folder_list(&json!([{"folder_id": b, "name": "B", "path": null}]));
        assert_eq!(s.view().folders.len(), 2);
        s.apply_event(&json!({"type": "purge_completed", "folder_id": a}));
        assert_eq!(s.view().folders.len(), 1);
        // The service's own flag (survives an app restart).
        s.apply_snapshot(&snap(json!([
            {"folder_id": b, "display_name": "B", "status": "synced", "files_indexed": 1,
             "files_total": 1, "removing": true}
        ])));
        assert_eq!(s.view().folders[0].state, FolderState::Removing);
        assert_eq!(s.view().overall, Overall::Synced);
    }

    #[test]
    fn fullscreen_wait_and_catch_up_flags() {
        let id = FolderId::new();
        let mut s = ViewState::new();
        s.apply_snapshot(&snap(json!([{
            "folder_id": id, "display_name": "Docs", "status": "indexing",
            "files_indexed": 340, "files_total": 2100
        }])));
        s.apply_event(&json!({"type": "index_progress", "folder_id": id, "files_done": 340, "files_total": 2100, "eta_s": 240}));
        assert!(
            s.apply_event(&json!({"type": "gpu.fullscreen_wait", "waiting": true}))
                .changed
        );
        let v = s.view();
        assert!(v.waiting_for_fullscreen);
        assert_eq!(v.summary, "Waiting while a full-screen app is open");
        assert_eq!(
            v.folders[0].line,
            "Indexing 340 of 2,100 files · Waiting while a full-screen app is open"
        );
        s.apply_event(&json!({"type": "gpu.fullscreen_wait", "waiting": false}));
        let v = s.view();
        assert_eq!(v.summary, "Indexing 340 of 2,100 files");
        assert!(!v.catching_up);
        s.apply_event(&json!({"type": "catch_up_progress", "done": 1, "total": 4}));
        assert!(s.view().catching_up);
    }

    #[test]
    fn approvals_dedupe_expiry_and_focus() {
        let mut s = ViewState::new();
        s.apply_snapshot(&snap(json!([])));
        assert!(s.set_focus_approval("01A".into()));
        let list = json!([
            {"approval_id": "01A", "client": "claude_desktop", "path": r"C:\Docs", "expires_at": 1_700_000_600_000_i64},
            {"approval_id": "01B", "client": "claude_desktop", "path": r"c:\docs", "expires_at": 1_700_000_700_000_i64},
            {"approval_id": "01C", "client": "cursor", "path": r"C:\Docs", "expires_at": 0}
        ]);
        assert!(s.apply_approvals(&list));
        let v = s.view();
        assert_eq!(v.approvals.len(), 2, "same client + folder = one card");
        assert_eq!(v.approvals[0].client, "Claude");
        assert!((v.approvals[0].expires_at_ms - 1_700_000_600_000.0).abs() < 1.0);
        assert_eq!(v.focus_approval.as_deref(), Some("01A"));
        s.remove_approval("01A");
        assert_eq!(s.view().focus_approval, None);
        // A new request while the app runs brings its card into view and lists it.
        let id = sr_common::ids::ApprovalId::new();
        let fx = s.apply_event(
            &json!({"type": "approval_requested", "approval_id": id, "client": "claude_desktop"}),
        );
        assert!(fx.changed && fx.refresh_approvals);
        assert_eq!(s.view().focus_approval, Some(id.to_string()));
    }

    #[test]
    fn consent_requests_and_provider_health() {
        let mut s = ViewState::new();
        s.apply_snapshot(&snap(json!([])));
        s.apply_event(
            &json!({"type": "consent.needed", "provider": "someprov", "purpose": "iterate"}),
        );
        let r = s.view().consent_request.unwrap();
        assert_eq!(
            r.purpose,
            ConsentPurpose::Answer,
            "iterate shares the answer consent"
        );
        assert_eq!(r.provider, "someprov");
        // An unrelated purpose doesn't close the dialog; the matching one does.
        s.apply_event(&json!({"type": "consent.changed", "provider": "someprov", "purpose": "enhance", "allowed": true}));
        assert!(s.view().consent_request.is_some());
        s.apply_event(&json!({"type": "consent.changed", "provider": "someprov", "purpose": "answer", "allowed": true}));
        assert!(s.view().consent_request.is_none());
        s.apply_event(
            &json!({"type": "provider_health", "provider": "someprov", "health": "down"}),
        );
        assert_eq!(s.view().provider_health, Some(ProviderHealthView::Down));
    }

    #[test]
    fn m5_policy_index_missing_and_onedrive_reach_the_view() {
        let id = FolderId::new();
        let mut s = ViewState::new();
        // Recovery mode (R34): the problem screen, its title as the summary, no generic panel.
        s.apply_snapshot(&json!({"host": "starting", "folders": [], "queued_jobs": 0,
            "summary": "Search index not found",
            "index_missing": {"path": r"D:\SeleneRetrieval Index", "drive": "D:",
                              "old_copy": null, "folders_to_restore": 2},
            "managed": [{"path": "integrations.http_server_enabled",
                         "message": "Managed by your organisation"}]}));
        let v = s.view();
        assert_eq!(v.overall, Overall::Problem);
        assert!(v.problem.is_none());
        let m = v.index_missing.clone().unwrap();
        assert_eq!(v.summary, m.error.title);
        assert_eq!(m.folders_to_restore, 2);
        assert!(m.old_copy.is_none());
        assert!(v.policy.is_managed("integrations.http_server_enabled"));
        // The choice was sent: "Restarting…" until the new run's snapshot.
        assert!(s.clear_index_missing());
        s.set_restarting(true);
        assert_eq!(s.view().overall, Overall::Starting);
        s.apply_snapshot(&snap(json!([{"folder_id": id, "display_name": "OneDrive",
            "status": "synced", "files_indexed": 9, "files_total": 9,
            "online_only_skipped": 3, "cloud_note": "This folder is in OneDrive."}])));
        let v = s.view();
        assert!(v.index_missing.is_none() && v.policy.managed.is_empty());
        assert_eq!(v.folders[0].online_only_skipped, 3);
        assert_eq!(
            v.folders[0].cloud_note.as_deref(),
            Some("This folder is in OneDrive.")
        );
    }

    #[test]
    fn text_helpers() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(1234567), "1,234,567");
        assert_eq!(duration_text(10), "a minute or two");
        assert_eq!(duration_text(90), "a minute or two");
        assert_eq!(duration_text(600), "about 10 minutes");
        assert_eq!(duration_text(4000), "about an hour");
        assert_eq!(duration_text(3 * 3600 + 1), "about 4 hours");
    }
}
