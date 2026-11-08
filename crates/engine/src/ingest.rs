//! The ingest executor (docs/02 §2.3): one job at a time per permit.
//!
//! `index_file`: catalog row → [`SandboxFs`] handle (the worker never gets a path) → content hash
//! (unchanged files stop here) → sandboxed `sr-extract` worker → chunker (bundled bge-small
//! tokenizer, 400/48) → **T1** (`write_file_chunks`: version++, chunks, chunk_text, outbox op) →
//! nudge the IndexWriter. The writer embeds and commits on its own task (group commit).
//!
//! Failures follow the extract supervisor's disposition: `DeadLetter` → the job fails
//! non-retryably and the catalog dead-letters the file (`synced_with_errors`, "View skipped
//! files"); `Retry` → retryable failure with backoff (AV locks, U20).

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{Arc, OnceLock};

use sr_catalog::repo::chunks::{FileChunks, NewChunk};
use sr_catalog::repo::{chunks, files, folders, jobs, outbox};
use sr_catalog::{Catalog, FileState, Job, JobKind};
use sr_common::error::{Error, ErrorCode};
use sr_common::events::EngineEvent;
use sr_common::ids::{ContentHash, FileId};
use sr_core::ingest::{Chunker, ContentKind};
use sr_extract_client::{Disposition, ExtractPool, ExtractSource, kind_for_path};
use sr_sync::{JobExecutor, JobOutcome};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::bus::EventBus;
use crate::clock::SharedClock;
use crate::sandboxfs::SharedSandbox;

/// Extractor version recorded with each file version (part of `chunk_cfg_fp`).
pub fn extractor_version() -> String {
    format!(
        "sr-extract/p{}",
        sr_common::protocol::extract::EXTRACT_PROTO
    )
}

/// Everything the executor needs.
pub struct Ingest {
    pub(crate) cat: Arc<Catalog>,
    pub(crate) clock: SharedClock,
    pub(crate) bus: Arc<EventBus>,
    pub(crate) sandbox: SharedSandbox,
    pub(crate) chunker: Arc<Chunker>,
    pub(crate) extract: Arc<OnceLock<Arc<ExtractPool>>>,
    pub(crate) writer_nudge: Arc<Notify>,
}

fn file_name(rel: &str) -> String {
    Path::new(rel)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn not_found(e: &Error) -> bool {
    e.code == ErrorCode::NotFound
}

impl Ingest {
    fn sandbox(&self) -> Arc<crate::sandboxfs::SandboxFs> {
        self.sandbox
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Index (or re-index) one file. `Ok(Done)` also for files that vanished or did not change.
    /// `force` re-indexes unchanged content (`reindex_policy = always`).
    pub async fn index_file(&self, file_id: FileId, force: bool) -> Result<JobOutcome, Error> {
        let rec = self
            .cat
            .read_async(move |c| {
                let f = files::file(c, file_id)?;
                let fo = folders::folder(c, f.folder_id)?;
                Ok((f, fo))
            })
            .await;
        let (file, folder) = match rec {
            Ok(v) => v,
            Err(e) if not_found(&e) => return Ok(JobOutcome::Done),
            Err(e) => return Err(e),
        };
        let name = file_name(&file.rel_path);
        if !folder.available {
            return Err(Error::new(ErrorCode::FolderUnavailable)
                .with_name(folder.settings.display_name.clone())
                .with_retry_after(60));
        }
        let sb = self.sandbox();
        let loc = match sb.resolve_in(folder.id, &file.rel_path) {
            Ok(l) => l,
            Err(e) => return self.policy_or_gone(file_id, e).await,
        };
        // M5-E: the file turned online-only after the scan (OneDrive "Free up space"): skip it
        // instead of downloading it (unless the folder includes online-only files on purpose).
        // Hydrating it again re-indexes it through the watcher.
        if !folder.settings.include_online_only && crate::win::is_online_only(&loc.resolved.path) {
            let now = self.clock.now_ms();
            self.cat
                .write(move |c| {
                    files::mark_skipped(
                        c,
                        file_id,
                        sr_catalog::repo::status::SKIP_REASON_ONLINE_ONLY,
                        Some(ErrorCode::FileOnlineOnly),
                        now,
                    )
                })
                .await?;
            self.writer_nudge.notify_one();
            return Ok(JobOutcome::Done);
        }
        let mut handle = match sb.open(&loc) {
            Ok(f) => f,
            Err(e) => return self.policy_or_gone(file_id, e).await,
        };
        let size = handle
            .metadata()
            .map_err(|e| Error::from(e).with_name(&name))?
            .len();
        if size > folder.settings.max_file_bytes {
            let now = self.clock.now_ms();
            self.cat
                .write(move |c| {
                    files::mark_skipped(c, file_id, "too_large", Some(ErrorCode::FileTooLarge), now)
                })
                .await?;
            self.bus.publish(EngineEvent::FileFailed {
                folder_id: folder.id,
                file_id,
                code: ErrorCode::FileTooLarge,
            });
            self.writer_nudge.notify_one();
            return Ok(JobOutcome::Done);
        }
        // Content hash from the handle (and, for JSON, the line count of the data-file rule),
        // then rewind it for the worker.
        let count_lines = sr_sync::filter::DATA_FILE_EXTS.contains(&file.ext.as_str());
        let (handle, hash, lines) = tokio::task::spawn_blocking(move || {
            let mut h = blake3::Hasher::new();
            let mut buf = vec![0u8; 64 * 1024];
            let (mut lines, mut last) = (0u64, b'\n');
            loop {
                let n = handle.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
                if count_lines {
                    lines += buf[..n].iter().filter(|b| **b == b'\n').count() as u64;
                    last = buf[n - 1];
                }
            }
            if last != b'\n' {
                lines += 1;
            }
            handle.seek(SeekFrom::Start(0))?;
            Ok::<_, std::io::Error>((
                handle,
                ContentHash::from_bytes(*h.finalize().as_bytes()),
                lines,
            ))
        })
        .await
        .map_err(|e| Error::new(ErrorCode::Internal).with_source(e))?
        .map_err(|e| Error::from(e).with_name(&name))?;

        // docs/06 change 5: a JSON data dump (lines averaging over 1,000 bytes) is skipped, not
        // indexed, unless a `.seleneignore` `!pattern` re-includes the file.
        if sr_sync::filter::is_data_dump(&file.ext, size, lines)
            && !sr_sync::rules::FolderRules::new(&folder.root_path)
                .reincluded(&file.rel_path, false)
        {
            let now = self.clock.now_ms();
            self.cat
                .write(move |c| {
                    files::mark_skipped(c, file_id, sr_sync::reconcile::SKIP_DATA_FILE, None, now)
                })
                .await?;
            self.writer_nudge.notify_one();
            return Ok(JobOutcome::Done);
        }

        let ext_version = extractor_version();
        let ckind = ContentKind::from_extension(&file.ext);
        let cfg_fp = self.chunker.cfg_fp(ckind, &ext_version);
        if !force
            && file.state == FileState::Indexed
            && file.content_hash == Some(hash)
            && file.chunk_cfg_fp == Some(cfg_fp)
        {
            return Ok(JobOutcome::Done); // unchanged: nothing to embed
        }
        let Some(pool) = self.extract.get().cloned() else {
            return Err(Error::new(ErrorCode::Internal)
                .with_detail("file reader (sr-extract) is not available")
                .with_retry_after(60));
        };
        let kind = kind_for_path(Path::new(&file.rel_path));
        let x = match pool.extract(ExtractSource::File(handle), kind).await {
            Ok(x) => x,
            Err(e) => {
                let dead = e.disposition == Disposition::DeadLetter;
                let mut err = e.with_path(loc.display_path()).error;
                err.retryable = !dead;
                if dead {
                    self.bus.publish(EngineEvent::FileFailed {
                        folder_id: folder.id,
                        file_id,
                        code: err.code,
                    });
                }
                return Err(err);
            }
        };
        let chunker = self.chunker.clone();
        let text = x.text;
        let ev = ext_version.clone();
        let pieces =
            tokio::task::spawn_blocking(move || chunker.chunk(&text, ckind, file_id, &hash, &ev))
                .await
                .map_err(|e| Error::new(ErrorCode::Internal).with_source(e))??;
        let fc = FileChunks {
            file_id,
            content_hash: hash,
            chunk_cfg_fp: cfg_fp,
            extractor_version: ext_version,
            chunks: pieces
                .into_iter()
                .map(|c| NewChunk {
                    ord: c.ord,
                    byte_start: c.byte_start,
                    byte_end: c.byte_end,
                    line_start: c.line_start,
                    line_end: c.line_end,
                    tokens: c.tokens,
                    text: c.text,
                })
                .collect(),
        };
        let now = self.clock.now_ms();
        self.cat
            .write(move |c| chunks::write_file_chunks(c, &fc, now))
            .await?;
        self.writer_nudge.notify_one();
        Ok(JobOutcome::Done)
    }

    /// A file that no longer exists is removed from the catalog (with its outbox op); a policy
    /// refusal (denylist added later, junction now pointing outside) dead-letters it.
    async fn policy_or_gone(&self, file_id: FileId, e: Error) -> Result<JobOutcome, Error> {
        let now = self.clock.now_ms();
        if e.code == ErrorCode::NotFound {
            self.cat
                .write(move |c| files::delete_file(c, file_id, now))
                .await?;
            self.writer_nudge.notify_one();
            return Ok(JobOutcome::Done);
        }
        let mut e = e;
        if matches!(
            e.code,
            ErrorCode::PathDenied | ErrorCode::PathOutsideSandbox | ErrorCode::InvalidInput
        ) {
            e.retryable = false;
        }
        Err(e)
    }

    /// Delete one file (watcher event).
    pub async fn delete_file(&self, file_id: FileId) -> Result<JobOutcome, Error> {
        let now = self.clock.now_ms();
        self.cat
            .write(move |c| files::delete_file(c, file_id, now))
            .await?;
        self.writer_nudge.notify_one();
        Ok(JobOutcome::Done)
    }

    /// Path tokens after a rename (m1-e O3): re-apply the file at its current version, so the
    /// lexical index re-adds its docs with the new path. Vectors are copied by `text_hash`
    /// (nothing is re-embedded).
    pub async fn refresh_paths(&self, ids: Vec<FileId>) -> Result<usize, Error> {
        if ids.is_empty() {
            return Ok(0);
        }
        let now = self.clock.now_ms();
        let n = self
            .cat
            .write(move |c| {
                sr_catalog::atomic(c, |c| {
                    let mut n = 0;
                    for id in &ids {
                        let Ok(f) = files::file(c, *id) else { continue };
                        if f.version == 0 || f.state != FileState::Indexed {
                            continue; // a pending op will carry the new path anyway
                        }
                        let col = folders::folder(c, f.folder_id)?.collection_id;
                        outbox::append(
                            c,
                            col,
                            None,
                            None,
                            Some(*id),
                            sr_catalog::model::IndexOpKind::Upsert,
                            f.version,
                            now,
                        )?;
                        n += 1;
                    }
                    Ok(n)
                })
            })
            .await?;
        if n > 0 {
            self.writer_nudge.notify_one();
        }
        Ok(n)
    }
}

/// What the claim loop hands the executor (sr-sync runs `scan_folder` and `delete_file` itself).
impl JobExecutor for Ingest {
    async fn execute(&self, job: Job, cancel: &CancellationToken) -> sr_common::Result<JobOutcome> {
        if cancel.is_cancelled() {
            return Err(Error::new(ErrorCode::Cancelled));
        }
        let flag = |k: &str| {
            job.payload
                .as_ref()
                .and_then(|p| p.get(k))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        };
        match (job.kind, job.file_id) {
            (JobKind::IndexFile, Some(f)) if flag(jobs::PAYLOAD_PATH_REFRESH) => {
                self.refresh_paths(vec![f]).await?;
                Ok(JobOutcome::Done)
            }
            (JobKind::IndexFile, Some(f)) => {
                let work = self.index_file(f, flag(jobs::PAYLOAD_FORCE));
                let r = tokio::select! {
                    r = work => r,
                    () = cancel.cancelled() => Err(Error::new(ErrorCode::Cancelled)),
                };
                // Renamed and (as it turned out) unchanged: the content path wrote no new
                // version, so the lexical path tokens still need the refresh. A changed file got a
                // new version, whose outbox op carries the current path; `refresh_paths` skips it
                // (state `pending_index`).
                if r.is_ok() && flag(jobs::PAYLOAD_RENAMED) {
                    self.refresh_paths(vec![f]).await?;
                }
                r
            }
            (JobKind::DeleteFile, Some(f)) => self.delete_file(f).await,
            // Purge is applied by the IndexWriter from the outbox; retention runs in the engine's
            // maintenance task; rebuilds (shadow re-embed) are M2.
            _ => Ok(JobOutcome::Done),
        }
    }
}
