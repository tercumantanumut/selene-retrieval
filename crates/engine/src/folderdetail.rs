//! Folder detail (M4-E, page 25; OQ-37/38/39): status, last run, file counts, the skipped list
//! with plain reasons and per-file retry, and the folder's settings.

use sr_catalog::FileState;
use sr_catalog::repo::{files, folders, groups, status};
use sr_common::api::{
    FileCounts, FolderDetail, LastRun, SKIPPED_PAGE_DEFAULT, SKIPPED_PAGE_MAX, SkipReason,
    SkippedFile, SkippedPage,
};
use sr_common::config::FolderSettings;
use sr_common::error::{Error, ErrorCode};
use sr_common::ids::{FileId, FolderId};

use crate::EngineHandle;

fn skipped_file(r: files::SkippedRow) -> SkippedFile {
    let code = r
        .error_code
        .as_deref()
        .and_then(|s| ErrorCode::ALL.iter().copied().find(|c| c.as_str() == s));
    let reason = SkipReason::classify(r.skip_reason.as_deref(), code);
    SkippedFile {
        file_id: r.id,
        rel_path: r.rel_path,
        reason,
        code,
        // Only dead files are retried (a size limit or an online-only file won't change).
        retryable: r.state == FileState::Dead && reason.retryable(),
        size: r.size,
        updated_at: r.updated_at,
    }
}

impl EngineHandle {
    /// `folders.detail`.
    pub async fn folder_detail(&self, id: FolderId) -> Result<FolderDetail, Error> {
        let info = self
            .list_folders()
            .await?
            .into_iter()
            .find(|f| f.folder_id == id && !f.removing)
            .ok_or_else(|| Error::new(ErrorCode::NotFound))?;
        let default_group = self.default_group();
        self.inner()
            .cat
            .read_async(move |c| {
                let f = folders::folder(c, id)?;
                let n = status::counts(c, id)?;
                let gid = groups::group_of(c, id)?;
                let gid = if gid.to_bytes() == [0; 16] {
                    default_group
                } else {
                    gid
                };
                let group_name = groups::group(c, gid).map(|g| g.name).unwrap_or_default();
                let (total, rows) = files::skipped_page(c, id, 0, SKIPPED_PAGE_DEFAULT)?;
                Ok(FolderDetail {
                    folder: info,
                    settings: f.settings.clone(),
                    group_id: gid,
                    group_name,
                    paused: f.paused,
                    last_run: LastRun {
                        trigger: f.last_run_trigger.map(|t| t.as_str().to_string()),
                        last_full_scan_at: f.last_full_scan_at,
                        last_success_at: f.last_success_at,
                        partial: f.last_scan_partial,
                        stopped: f.cancelled_at.is_some(),
                    },
                    counts: FileCounts {
                        indexed: n.indexed_files,
                        pending: n.pending_files,
                        skipped: n.skipped_files + n.dead_files,
                        listed: n.listed_files,
                    },
                    skipped: rows.into_iter().map(skipped_file).collect(),
                    skipped_total: total,
                })
            })
            .await
    }

    /// `folders.skipped`: one page of the skipped list, newest first.
    pub async fn folder_skipped(
        &self,
        id: FolderId,
        offset: Option<u32>,
        limit: Option<u32>,
    ) -> Result<SkippedPage, Error> {
        let limit = limit
            .unwrap_or(SKIPPED_PAGE_DEFAULT)
            .clamp(1, SKIPPED_PAGE_MAX);
        let offset = offset.unwrap_or(0);
        self.inner()
            .cat
            .read_async(move |c| {
                folders::folder(c, id)?;
                let (total, rows) = files::skipped_page(c, id, offset, limit)?;
                Ok(SkippedPage {
                    total,
                    files: rows.into_iter().map(skipped_file).collect(),
                })
            })
            .await
    }

    /// `folders.retry_files`: dead files (all retryable ones, or the named ones) are queued again
    /// with a fresh attempt budget. Returns how many.
    pub async fn retry_files(&self, id: FolderId, only: Option<Vec<FileId>>) -> Result<u32, Error> {
        let now = self.inner().clock.now_ms();
        let queued = self
            .inner()
            .cat
            .write(move |c| {
                folders::folder(c, id)?;
                files::retry_dead(c, id, only.as_deref(), now)
            })
            .await?;
        if !queued.is_empty() {
            self.inner().sync.wake();
            self.refresh_statuses(Some(vec![id])).await;
        }
        Ok(u32::try_from(queued.len()).unwrap_or(u32::MAX))
    }

    /// `folders.update_settings`: the whole settings object (bounds checked). Returns whether the
    /// folder is re-read (`reindex`) and the fresh detail.
    pub async fn update_folder_settings(
        &self,
        id: FolderId,
        settings: FolderSettings,
    ) -> Result<(bool, FolderDetail), Error> {
        if self
            .inner()
            .maint
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .removing
            .contains_key(&id)
        {
            return Err(Error::new(ErrorCode::Conflict));
        }
        // The same lenient rules as loading: a value out of bounds is refused, never stored.
        let (checked, issues) = FolderSettings::from_json_lenient(
            &serde_json::to_value(&settings)
                .map_err(|e| Error::new(ErrorCode::Internal).with_source(e))?,
        );
        if !issues.is_empty() {
            return Err(Error::new(ErrorCode::InvalidInput).with_detail(format!(
                "invalid folder settings: {}",
                issues
                    .iter()
                    .map(|i| i.path.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        let reindex = self.inner().sync.update_settings(id, checked).await?;
        self.revalidate_roots().await?;
        self.inner().sync.folders_changed();
        self.inner()
            .bus
            .publish(sr_common::events::EngineEvent::SettingsChanged);
        let detail = self.folder_detail(id).await?;
        Ok((reindex, detail))
    }
}
