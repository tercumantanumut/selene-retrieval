//! Folder detail, per-folder settings, skipped files and folder groups (pages 24, 25, 31), over
//! the M4 contract (`docs/research/m4-rpc-contract.md` §3, §6). Plain logic over [`Rpc`], so it is
//! unit-tested with the fake service; `commands.rs` only forwards.
//!
//! View models translate the service's shapes into plain words (skip reasons from page 25's table)
//! and into IPC-safe numbers (no 64-bit integers cross into TypeScript).
#![allow(clippy::result_large_err)] // `UiError` is the Tauri IPC error (see commands.rs).

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use specta::Type;
use sr_common::api::{FolderDetail, GroupView, SkipReason, SkippedFile, SkippedPage};
use sr_common::config::FolderSettings;
use sr_common::error::{Error, ErrorCode};

use crate::daemon::{Rpc, method};
use crate::dto::UiError;

fn u32_of(n: u64) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

#[allow(clippy::cast_precision_loss)] // ms timestamps stay far below 2^53
fn ms(t: i64) -> f64 {
    t as f64
}

// ---------------------------------------------------------------------------------------------
// Settings (same field and value names as `sr_common::config::FolderSettings`; converted with
// serde, so a new engine field only needs a field here)

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum FileTypes {
    Documents,
    Code,
    DocumentsAndCode,
    AllText,
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum SyncMode {
    Auto,
    Manual,
    Scheduled,
    Triggered,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum ReindexPolicy {
    Smart,
    Always,
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum ChunkPreset {
    Balanced,
    Small,
    Large,
    Custom,
}

/// One folder's settings (page 25). Labels are the doc comments of `FolderSettings`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct FolderSettingsView {
    /// Name shown in the app and to connected apps. Empty = the folder's own name.
    pub display_name: String,
    /// Keep this folder up to date automatically.
    pub keep_up_to_date: bool,
    /// Which kinds of files to include.
    pub file_types_preset: FileTypes,
    /// Include OneDrive files that are only stored online (downloads them). M5 in the UI.
    pub include_online_only: bool,
    /// When to re-check the folder (used when "keep up to date" is on).
    pub sync_mode: SyncMode,
    /// Full re-check interval in minutes (5-1440).
    pub cadence_min: u32,
    /// When to re-index changed files.
    pub reindex_policy: ReindexPolicy,
    /// Include subfolders.
    pub recursive: bool,
    /// Extensions to include when the preset is "custom" (without the dot, e.g. "md").
    pub include_extensions: Vec<String>,
    /// Extra glob patterns to exclude.
    pub exclude_globs: Vec<String>,
    /// Skip files larger than this many bytes (1 KiB-200 MiB).
    pub max_file_bytes: u32,
    /// Chunk size preset.
    pub chunk_preset: ChunkPreset,
    /// Custom chunk size in tokens (64-8192).
    pub chunk_size_override: Option<u32>,
    /// Custom chunk overlap in tokens (0-2048, smaller than the chunk size).
    pub chunk_overlap_override: Option<u32>,
    /// Follow symbolic links (never outside the folder).
    pub follow_symlinks: bool,
}

pub fn settings_view(s: &FolderSettings) -> FolderSettingsView {
    serde_json::to_value(s)
        .ok()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_else(|| {
            // Unreachable while the two shapes match; fall back to the defaults' view.
            serde_json::from_value(
                serde_json::to_value(FolderSettings::default()).unwrap_or(Value::Null),
            )
            .unwrap_or_else(|_| default_view())
        })
}

fn default_view() -> FolderSettingsView {
    FolderSettingsView {
        display_name: String::new(),
        keep_up_to_date: true,
        file_types_preset: FileTypes::DocumentsAndCode,
        include_online_only: false,
        sync_mode: SyncMode::Auto,
        cadence_min: 60,
        reindex_policy: ReindexPolicy::Smart,
        recursive: true,
        include_extensions: Vec::new(),
        exclude_globs: Vec::new(),
        max_file_bytes: 10 * 1024 * 1024,
        chunk_preset: ChunkPreset::Balanced,
        chunk_size_override: None,
        chunk_overlap_override: None,
        follow_symlinks: false,
    }
}

/// The view back into the engine's shape (lenient: bad fields are reset by the engine, which
/// answers `InvalidInput` for out-of-bounds values).
pub fn settings_value(v: &FolderSettingsView) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

/// Field-level problems for live validation (DS-4.18), in plain words. Empty = fine.
pub fn validate(v: &FolderSettingsView) -> Vec<FieldProblem> {
    let mut out = Vec::new();
    let mut bad = |field: &str, text: &str| {
        out.push(FieldProblem {
            field: field.into(),
            text: text.into(),
        });
    };
    if v.display_name.chars().count() > 200 || v.display_name.chars().any(char::is_control) {
        bad("display_name", "Use a shorter name.");
    }
    if !(5..=1440).contains(&v.cadence_min) {
        bad("cadence_min", "Use a number from 5 to 1440.");
    }
    if !(1024..=200 * 1024 * 1024).contains(&v.max_file_bytes) {
        bad("max_file_bytes", "Use a size from 1 KB to 200 MB.");
    }
    let ext_ok = |e: &str| {
        (1..=16).contains(&e.len()) && e.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    if v.include_extensions.iter().any(|e| !ext_ok(e)) {
        bad(
            "include_extensions",
            "Use letters and digits only, without the dot (for example md).",
        );
    }
    if v.exclude_globs
        .iter()
        .any(|g| g.is_empty() || g.len() > 1024)
    {
        bad("exclude_globs", "Remove the empty or very long pattern.");
    }
    if let Some(n) = v.chunk_size_override
        && !(64..=8192).contains(&n)
    {
        bad("chunk_size_override", "Use a number from 64 to 8192.");
    }
    if let Some(o) = v.chunk_overlap_override {
        let size = v.chunk_size_override.unwrap_or(u32::MAX);
        if o > 2048 || o >= size {
            bad(
                "chunk_overlap_override",
                "Use a number from 0 to 2048, smaller than the chunk size.",
            );
        }
    }
    out
}

/// What a settings change will do, so the UI can confirm **before** saving (page 25).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum ChangeImpact {
    /// Saves at once.
    None,
    /// "This re-reads the folder. Search keeps working meanwhile."
    Reread,
}

pub fn change_impact(before: &FolderSettingsView, after: &FolderSettingsView) -> ChangeImpact {
    let rereads = before.file_types_preset != after.file_types_preset
        || (after.file_types_preset == FileTypes::Custom
            && before.include_extensions != after.include_extensions)
        || before.exclude_globs != after.exclude_globs
        || before.max_file_bytes != after.max_file_bytes
        || before.recursive != after.recursive
        || before.follow_symlinks != after.follow_symlinks
        || before.chunk_preset != after.chunk_preset
        || before.chunk_size_override != after.chunk_size_override
        || before.chunk_overlap_override != after.chunk_overlap_override;
    if rereads {
        ChangeImpact::Reread
    } else {
        ChangeImpact::None
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct FieldProblem {
    pub field: String,
    pub text: String,
}

// ---------------------------------------------------------------------------------------------
// Detail and skipped files

/// One skipped file (page 25 table): reasons in plain words; listed files never appear.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct SkippedRow {
    pub file_id: String,
    pub rel_path: String,
    pub reason: String,
    pub retryable: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct SkippedPageView {
    pub total: u32,
    pub files: Vec<SkippedRow>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct FileCountsView {
    pub indexed: u32,
    pub pending: u32,
    pub skipped: u32,
    /// Listed, not read (never counted as skipped): an access grant's files, or those of a folder
    /// paused by Amendment 17 until the user chooses.
    pub listed: u32,
}

/// Page 25 (status, progress and the state line come live from `AppView.folders`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct FolderDetailView {
    pub id: String,
    pub name: String,
    pub path: String,
    pub available: bool,
    pub group_id: String,
    pub group_name: String,
    pub paused: bool,
    /// "Last checked {time}" (ms since the Unix epoch).
    pub last_checked_ms: Option<f64>,
    pub counts: FileCountsView,
    pub skipped: Vec<SkippedRow>,
    pub skipped_total: u32,
    pub settings: FolderSettingsView,
    /// Network / no-live-updates note (DS-5.3).
    pub watch_note: Option<String>,
    /// M5 (DS-5.3): files skipped because they are stored only in OneDrive. From the service's
    /// count (`FolderInfo.online_only_skipped`, M5-E); an older service: counted from the skipped rows shown.
    pub online_only_skipped: u32,
    /// M5: the service's OneDrive note for this folder (`cloud_note`).
    pub cloud_note: Option<String>,
}

/// "10 MB" (MiB shown as MB, as page 25 says).
pub fn size_mb(bytes: u64) -> String {
    let mb = bytes.div_ceil(1024 * 1024).max(1);
    format!("{mb} MB")
}

/// Page 25 reasons table (exact copy).
pub fn reason_text(r: SkipReason, max_file_bytes: u64) -> String {
    match r {
        SkipReason::TooLarge => format!("Bigger than the {} limit", size_mb(max_file_bytes)),
        SkipReason::OnlineOnly => "Stored only in OneDrive".into(),
        SkipReason::Unreadable => {
            "May be damaged, password-protected or in an unsupported format".into()
        }
        SkipReason::Timeout => "Took too long to read".into(),
        SkipReason::Permission => format!("{} didn't allow access", crate::dto::OS_NAME),
        SkipReason::Locked => "In use by another program; retried automatically".into(),
        SkipReason::DataFile => "Looks like a data file (very long lines), not a document".into(),
        SkipReason::Other => "Couldn't be read".into(),
    }
}

fn skipped_row(f: &SkippedFile, max_file_bytes: u64) -> SkippedRow {
    SkippedRow {
        file_id: f.file_id.to_string(),
        rel_path: f.rel_path.clone(),
        reason: reason_text(f.reason, max_file_bytes),
        retryable: f.retryable && f.reason.retryable(),
    }
}

pub fn detail_view(d: &FolderDetail) -> FolderDetailView {
    let max = d.settings.max_file_bytes;
    FolderDetailView {
        id: d.folder.folder_id.to_string(),
        name: d.folder.name.clone(),
        path: d.folder.path.clone(),
        available: d.folder.available,
        group_id: d.group_id.to_string(),
        group_name: d.group_name.clone(),
        paused: d.paused,
        last_checked_ms: d
            .last_run
            .last_success_at
            .or(d.folder.last_success_at)
            .map(ms),
        counts: FileCountsView {
            indexed: u32_of(d.counts.indexed),
            pending: u32_of(d.counts.pending),
            skipped: u32_of(d.counts.skipped),
            listed: u32_of(d.counts.listed),
        },
        skipped: d.skipped.iter().map(|f| skipped_row(f, max)).collect(),
        skipped_total: u32_of(d.skipped_total),
        settings: settings_view(&d.settings),
        watch_note: d.folder.watch_note.clone(),
        online_only_skipped: u32::try_from(
            d.skipped
                .iter()
                .filter(|f| matches!(f.reason, SkipReason::OnlineOnly))
                .count(),
        )
        .unwrap_or(u32::MAX),
        cloud_note: None,
    }
}

/// [`detail_view`] plus the M5 fields read from the raw reply (`folder.online_only_skipped`,
/// `folder.cloud_note`, published by M5-E; `policy::wire`).
fn detail_view_raw(d: &FolderDetail, raw: &Value) -> FolderDetailView {
    let mut v = detail_view(d);
    let folder = raw.get("folder").unwrap_or(&Value::Null);
    let (n, note) = crate::policy::cloud_fields(folder);
    let (n2, note2) = crate::policy::cloud_fields(raw);
    if let Some(n) = n.or(n2) {
        v.online_only_skipped = u32_of(n);
    }
    v.cloud_note = note.or(note2);
    v
}

fn parse<T: serde::de::DeserializeOwned>(v: Value) -> Result<T, UiError> {
    serde_json::from_value(v).map_err(|e| {
        UiError::from(Error::new(ErrorCode::Internal).with_detail(format!("unexpected reply: {e}")))
    })
}

pub async fn folder_detail<R: Rpc>(rpc: &R, folder_id: &str) -> Result<FolderDetailView, UiError> {
    let v = rpc
        .call(method::FOLDERS_DETAIL, json!({ "folder_id": folder_id }))
        .await
        .map_err(UiError::from)?;
    Ok(detail_view_raw(&parse::<FolderDetail>(v.clone())?, &v))
}

/// "Show all": the next page of skipped files (`limit` ≤ 500).
pub async fn skipped_files<R: Rpc>(
    rpc: &R,
    folder_id: &str,
    offset: u32,
    limit: u32,
    max_file_bytes: u32,
) -> Result<SkippedPageView, UiError> {
    let v = rpc
        .call(
            method::FOLDERS_SKIPPED,
            json!({ "folder_id": folder_id, "offset": offset, "limit": limit.clamp(1, 500) }),
        )
        .await
        .map_err(UiError::from)?;
    let page = parse::<SkippedPage>(v)?;
    Ok(SkippedPageView {
        total: u32_of(page.total),
        files: page
            .files
            .iter()
            .map(|f| skipped_row(f, u64::from(max_file_bytes)))
            .collect(),
    })
}

/// [Retry] one file, or [Retry all] (`None`). Returns how many were queued.
pub async fn retry_files<R: Rpc>(
    rpc: &R,
    folder_id: &str,
    file_ids: Option<Vec<String>>,
) -> Result<u32, UiError> {
    let mut p = json!({ "folder_id": folder_id });
    if let Some(ids) = file_ids {
        p["file_ids"] = json!(ids);
    }
    let v = rpc
        .call(method::FOLDERS_RETRY_FILES, p)
        .await
        .map_err(UiError::from)?;
    Ok(v.get("queued").and_then(Value::as_u64).map_or(0, u32_of))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct SettingsSaved {
    /// The change re-reads the folder (the UI confirmed it first).
    pub reindex: bool,
    pub detail: FolderDetailView,
}

/// Saves the whole settings object (as read from the detail). Invalid input never reaches the
/// service: the first plain field problem is returned instead.
pub async fn update_settings<R: Rpc>(
    rpc: &R,
    folder_id: &str,
    settings: &FolderSettingsView,
) -> Result<SettingsSaved, UiError> {
    if let Some(p) = validate(settings).first() {
        return Err(UiError::new(
            "Check this setting",
            &p.text,
            crate::dto::UiAction::None,
            "",
            &format!("InvalidInput · {}", p.field),
        ));
    }
    let v = rpc
        .call(
            method::FOLDERS_UPDATE_SETTINGS,
            json!({ "folder_id": folder_id, "settings": settings_value(settings) }),
        )
        .await
        .map_err(UiError::from)?;
    let reindex = v.get("reindex").and_then(Value::as_bool).unwrap_or(false);
    let raw = v.get("detail").cloned().unwrap_or(Value::Null);
    let detail = parse::<FolderDetail>(raw.clone())?;
    Ok(SettingsSaved {
        reindex,
        detail: detail_view_raw(&detail, &raw),
    })
}

/// [Stop] (OQ-39): stops the current run; search covers what's done.
pub async fn cancel_folder<R: Rpc>(rpc: &R, folder_id: &str) -> Result<(), UiError> {
    rpc.call(method::FOLDERS_CANCEL, json!({ "folder_id": folder_id }))
        .await
        .map(|_| ())
        .map_err(UiError::from)
}

// ---------------------------------------------------------------------------------------------
// Folder groups (page 31, Advanced only)

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct GroupRow {
    pub id: String,
    pub name: String,
    /// "My folders": can't be deleted; new folders go here.
    pub is_default: bool,
    pub folder_ids: Vec<String>,
}

fn group_row(g: &GroupView) -> GroupRow {
    GroupRow {
        id: g.group_id.to_string(),
        name: g.name.clone(),
        is_default: g.is_default,
        folder_ids: g.folder_ids.iter().map(ToString::to_string).collect(),
    }
}

pub async fn list_groups<R: Rpc>(rpc: &R) -> Result<Vec<GroupRow>, UiError> {
    let v = rpc
        .call(method::GROUPS_LIST, json!({}))
        .await
        .map_err(UiError::from)?;
    let groups: Vec<GroupView> = parse(v.get("groups").cloned().unwrap_or(json!([])))?;
    Ok(groups.iter().map(group_row).collect())
}

/// Plain field text for a group name, or `None` when it is fine (page 31: "Name taken").
pub fn group_name_problem(
    name: &str,
    existing: &[GroupRow],
    own_id: Option<&str>,
) -> Option<String> {
    let n = name.trim();
    if n.is_empty() {
        return Some("Enter a name.".into());
    }
    if n.chars().count() > sr_common::api::MAX_GROUP_NAME_CHARS {
        return Some("Use a shorter name.".into());
    }
    let taken = existing
        .iter()
        .any(|g| Some(g.id.as_str()) != own_id && g.name.trim().eq_ignore_ascii_case(n));
    taken.then(|| "There's already a group with this name.".into())
}

pub async fn create_group<R: Rpc>(rpc: &R, name: &str) -> Result<GroupRow, UiError> {
    let v = rpc
        .call(method::GROUPS_CREATE, json!({ "name": name.trim() }))
        .await
        .map_err(UiError::from)?;
    Ok(group_row(&parse::<GroupView>(v)?))
}

pub async fn rename_group<R: Rpc>(
    rpc: &R,
    group_id: &str,
    name: &str,
) -> Result<GroupRow, UiError> {
    let v = rpc
        .call(
            method::GROUPS_RENAME,
            json!({ "group_id": group_id, "name": name.trim() }),
        )
        .await
        .map_err(UiError::from)?;
    Ok(group_row(&parse::<GroupView>(v)?))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct GroupDeleted {
    pub moved_folders: u32,
    /// Apps that now see no folder at all (page 31 notice, fail closed).
    pub apps_without_access: Vec<String>,
}

pub async fn delete_group<R: Rpc>(rpc: &R, group_id: &str) -> Result<GroupDeleted, UiError> {
    let v = rpc
        .call(method::GROUPS_DELETE, json!({ "group_id": group_id }))
        .await
        .map_err(UiError::from)?;
    let apps = v
        .get("clients_without_access")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| c.get("display_name").and_then(Value::as_str))
        .map(str::to_owned)
        .collect();
    Ok(GroupDeleted {
        moved_folders: v
            .get("moved_folders")
            .and_then(Value::as_u64)
            .map_or(0, u32_of),
        apps_without_access: apps,
    })
}

pub async fn move_folder<R: Rpc>(rpc: &R, folder_id: &str, group_id: &str) -> Result<(), UiError> {
    rpc.call(
        method::GROUPS_MOVE_FOLDER,
        json!({ "folder_id": folder_id, "group_id": group_id }),
    )
    .await
    .map(|_| ())
    .map_err(UiError::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::fake::FakeRpc;
    use sr_common::ids::{FileId, FolderId, GroupId};

    fn detail_json(fid: &FolderId, gid: &GroupId) -> Value {
        json!({
            "folder": {"folder_id": fid, "name": "Docs", "path": "C:\\Docs", "status": "synced",
                "available": true, "files_indexed": 10, "files_total": 12, "files_skipped": 2,
                "last_success_at": 1_700_000_000_000_i64, "group_id": gid},
            "settings": serde_json::to_value(FolderSettings::default()).unwrap(),
            "group_id": gid, "group_name": "My folders", "paused": false,
            "last_run": {"trigger": "manual", "last_full_scan_at": null,
                "last_success_at": 1_700_000_100_000_i64, "partial": false, "stopped": false},
            "counts": {"indexed": 10, "pending": 0, "skipped": 2, "listed": 0},
            "skipped": [
                {"file_id": FileId::new(), "rel_path": "big.pdf", "reason": "too_large",
                 "code": "FileTooLarge", "retryable": false, "size": 50_000_000, "updated_at": 1},
                {"file_id": FileId::new(), "rel_path": "locked.docx", "reason": "locked",
                 "code": "FileLocked", "retryable": true, "size": 10, "updated_at": 2}
            ],
            "skipped_total": 2
        })
    }

    #[test]
    fn settings_round_trip_through_the_view() {
        let s = FolderSettings::default();
        let v = settings_view(&s);
        assert_eq!(
            v,
            default_view(),
            "the mirror and the engine's defaults agree"
        );
        let back: FolderSettings = serde_json::from_value(settings_value(&v)).unwrap();
        assert_eq!(back, s);
    }

    #[tokio::test]
    async fn detail_maps_reasons_and_times() {
        let rpc = FakeRpc::default();
        let (fid, gid) = (FolderId::new(), GroupId::new());
        rpc.reply(Ok(detail_json(&fid, &gid)));
        let d = folder_detail(&rpc, &fid.to_string()).await.unwrap();
        assert_eq!(rpc.calls()[0].0, "folders.detail");
        assert_eq!(d.group_name, "My folders");
        assert_eq!(d.last_checked_ms, Some(1_700_000_100_000.0));
        assert_eq!(d.skipped[0].reason, "Bigger than the 10 MB limit");
        assert!(!d.skipped[0].retryable);
        assert_eq!(
            d.skipped[1].reason,
            "In use by another program; retried automatically"
        );
        assert!(d.skipped[1].retryable);
        // Not found: the plain code-2 message for "isn't in Selene Retrieval anymore".
        rpc.reply(Err(Error::new(ErrorCode::NotFound)));
        assert!(folder_detail(&rpc, "x").await.is_err());
    }

    #[test]
    fn every_reason_has_plain_text() {
        for r in [
            SkipReason::TooLarge,
            SkipReason::OnlineOnly,
            SkipReason::Unreadable,
            SkipReason::Timeout,
            SkipReason::Permission,
            SkipReason::Locked,
            SkipReason::DataFile,
            SkipReason::Other,
        ] {
            let t = reason_text(r, 1024);
            assert!(!t.is_empty() && !t.contains("Error"), "{r:?}");
        }
        assert_eq!(size_mb(200 * 1024 * 1024), "200 MB");
        assert_eq!(size_mb(1024), "1 MB");
    }

    #[test]
    fn impact_and_validation() {
        let a = default_view();
        let mut b = a.clone();
        b.display_name = "Work".into();
        b.keep_up_to_date = false;
        assert_eq!(change_impact(&a, &b), ChangeImpact::None);
        b.file_types_preset = FileTypes::Code;
        assert_eq!(change_impact(&a, &b), ChangeImpact::Reread);
        let mut c = a.clone();
        c.cadence_min = 2;
        c.include_extensions = vec![".md".into()];
        c.chunk_size_override = Some(100);
        c.chunk_overlap_override = Some(100);
        let fields: Vec<String> = validate(&c).into_iter().map(|p| p.field).collect();
        assert_eq!(
            fields,
            [
                "cadence_min",
                "include_extensions",
                "chunk_overlap_override"
            ]
        );
        assert!(validate(&a).is_empty());
    }

    #[tokio::test]
    async fn invalid_settings_never_reach_the_service() {
        let rpc = FakeRpc::default();
        let mut v = default_view();
        v.cadence_min = 0;
        let e = update_settings(&rpc, "f", &v).await.unwrap_err();
        assert_eq!(e.body, "Use a number from 5 to 1440.");
        assert!(rpc.calls().is_empty());
    }

    #[tokio::test]
    async fn update_settings_sends_the_whole_object() {
        let rpc = FakeRpc::default();
        let (fid, gid) = (FolderId::new(), GroupId::new());
        rpc.reply(Ok(
            json!({"reindex": true, "detail": detail_json(&fid, &gid)}),
        ));
        let mut v = default_view();
        v.file_types_preset = FileTypes::Code;
        let r = update_settings(&rpc, &fid.to_string(), &v).await.unwrap();
        assert!(r.reindex);
        let (m, p) = &rpc.calls()[0];
        assert_eq!(m, "folders.update_settings");
        assert_eq!(p["settings"]["file_types_preset"], "code");
        assert_eq!(p["settings"]["max_file_bytes"], 10 * 1024 * 1024);
    }

    #[tokio::test]
    async fn retry_and_stop_forward() {
        let rpc = FakeRpc::default();
        rpc.reply(Ok(json!({"queued": 3})));
        assert_eq!(retry_files(&rpc, "f", None).await.unwrap(), 3);
        rpc.reply(Ok(json!({"queued": 1})));
        retry_files(&rpc, "f", Some(vec!["a".into()]))
            .await
            .unwrap();
        cancel_folder(&rpc, "f").await.unwrap();
        let c = rpc.calls();
        assert_eq!(c[0].1, json!({"folder_id": "f"}));
        assert_eq!(c[1].1, json!({"folder_id": "f", "file_ids": ["a"]}));
        assert_eq!(c[2].0, "folders.cancel");
    }

    #[test]
    fn group_names() {
        let g = vec![GroupRow {
            id: "1".into(),
            name: "My folders".into(),
            is_default: true,
            folder_ids: vec![],
        }];
        assert_eq!(
            group_name_problem("my FOLDERS ", &g, None).as_deref(),
            Some("There's already a group with this name.")
        );
        assert!(
            group_name_problem("My folders", &g, Some("1")).is_none(),
            "own name"
        );
        assert!(group_name_problem("  ", &g, None).is_some());
        assert!(group_name_problem("Work", &g, None).is_none());
    }

    #[tokio::test]
    async fn delete_group_lists_apps_left_without_access() {
        let rpc = FakeRpc::default();
        rpc.reply(Ok(json!({"moved_folders": 2, "clients_without_access": [
            {"client_id": "cursor", "kind": "cursor", "display_name": "Cursor",
             "scope": {"mode": "groups", "group_ids": []}, "first_seen_at": null,
             "last_seen_at": null, "sessions_open": 0}
        ]})));
        let r = delete_group(&rpc, "g").await.unwrap();
        assert_eq!(r.moved_folders, 2);
        assert_eq!(r.apps_without_access, ["Cursor"]);
    }
}
