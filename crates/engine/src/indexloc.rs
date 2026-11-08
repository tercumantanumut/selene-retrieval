//! `index_location.*` app-RPC (M4-E, page 32) over [`crate::indexmove`]: status, target check,
//! saving a move (the copy runs at the next daemon start) and cancelling a saved one.

use std::path::{Path, PathBuf};

use sr_catalog::repo::folders;
use sr_common::api::{IndexLocationStatus, IndexMoveCheck, IndexMoveProgress, MovePhase};
use sr_common::error::{Error, ErrorCode};

use crate::EngineHandle;
use crate::indexmove::{self, CheckInput, MovePlan, PlanState};

impl EngineHandle {
    fn index_size_blocking(root: PathBuf) -> u64 {
        indexmove::index_size(&root)
    }

    /// A saved move that hasn't run yet (shown as "waiting").
    fn waiting_move(&self) -> Option<IndexMoveProgress> {
        let plan = indexmove::read_plan(&self.inner().layout.anchor)?;
        (plan.state != PlanState::Switched).then(|| IndexMoveProgress {
            phase: MovePhase::Waiting,
            done_bytes: 0,
            total_bytes: 0,
            from: plan.from.display().to_string(),
            to: plan.to.display().to_string(),
        })
    }

    /// `index_location.status`.
    pub async fn index_location_status(&self) -> Result<IndexLocationStatus, Error> {
        let root = self.inner().layout.root.clone();
        let r2 = root.clone();
        let size = tokio::task::spawn_blocking(move || Self::index_size_blocking(r2))
            .await
            .map_err(|e| Error::new(ErrorCode::Internal).with_source(e))?;
        Ok(IndexLocationStatus {
            path: root.display().to_string(),
            is_default: self.inner().layout.is_default(),
            size_bytes: size,
            free_bytes: sr_providers::sys::free_disk(&root).unwrap_or(0),
            moving: self.waiting_move(),
            missing: None,
        })
    }

    async fn move_check(&self, target: String) -> Result<IndexMoveCheck, Error> {
        let roots: Vec<PathBuf> = self
            .inner()
            .cat
            .read_async(|c| {
                Ok(folders::folders(c)?
                    .into_iter()
                    .map(|f| f.root_path)
                    .collect())
            })
            .await?;
        let (anchor, current, app) = (
            self.inner().layout.anchor.clone(),
            self.inner().layout.root.clone(),
            self.inner().cfg.app_dir.clone(),
        );
        tokio::task::spawn_blocking(move || {
            let size = indexmove::index_size(&current);
            indexmove::check(&CheckInput {
                anchor: &anchor,
                current: &current,
                target: &target,
                folder_roots: &roots,
                app_dir: &app,
                size,
            })
        })
        .await
        .map_err(|e| Error::new(ErrorCode::Internal).with_source(e))?
    }

    /// `index_location.check`.
    pub async fn index_location_check(&self, target: String) -> Result<IndexMoveCheck, Error> {
        self.move_check(target).await
    }

    /// `index_location.move` (privileged): save the plan, then drain and exit; the next start
    /// copies, verifies and switches before the engine opens any store.
    pub async fn index_location_move(&self, target: String) -> Result<PathBuf, Error> {
        let anchor = self.inner().layout.anchor.clone();
        if indexmove::read_plan(&anchor).is_some() {
            return Err(Error::new(ErrorCode::Conflict).with_detail("a move is already saved"));
        }
        let c = self.move_check(target).await?;
        if let Some(p) = c.problem {
            return Err(
                Error::new(ErrorCode::InvalidInput).with_detail(format!("target refused: {p:?}"))
            );
        }
        if !c.enough_space {
            let mut e = Error::new(ErrorCode::DiskFull);
            e.ctx.need = Some(format!("{} MB", c.need_bytes.div_ceil(1_000_000)));
            e.ctx.drive = Some(c.drive.clone());
            return Err(e);
        }
        let to = PathBuf::from(&c.index_path);
        let plan = MovePlan {
            from: self.inner().layout.root.clone(),
            to: to.clone(),
            state: PlanState::Pending,
            created_at: self.inner().clock.now_ms(),
        };
        let a2 = anchor.clone();
        tokio::task::spawn_blocking(move || indexmove::write_plan(&a2, &plan))
            .await
            .map_err(|e| Error::new(ErrorCode::Internal).with_source(e))??;
        // `[system] data_dir` mirrors the new place for display (the pointer file is the truth).
        let shown = if self.is_anchor(&to) {
            None
        } else {
            Some(to.clone())
        };
        if let Err(e) = self
            .update_settings(move |s| s.system.data_dir = shown)
            .await
        {
            tracing::warn!(target: "sr_engine::indexmove", code = %e.code, "settings not updated for the move");
        }
        tracing::info!(target: "sr_engine::indexmove", "index move saved; restarting to run it");
        self.inner()
            .bus
            .publish(sr_common::events::EngineEvent::IndexMoveProgress(
                IndexMoveProgress {
                    phase: MovePhase::Waiting,
                    done_bytes: 0,
                    total_bytes: 0,
                    from: self.inner().layout.root.display().to_string(),
                    to: to.display().to_string(),
                },
            ));
        self.request_restart();
        Ok(to)
    }

    fn is_anchor(&self, p: &Path) -> bool {
        sr_client::naming::data_dir_key(p)
            == sr_client::naming::data_dir_key(&self.inner().layout.anchor)
    }

    /// `index_location.cancel` while the engine runs: drop a saved move that hasn't started.
    pub async fn index_location_cancel(&self) -> Result<bool, Error> {
        let anchor = self.inner().layout.anchor.clone();
        match indexmove::read_plan(&anchor) {
            Some(p) if p.state == PlanState::Pending => {
                indexmove::remove_plan(&anchor);
                let back = if self.inner().layout.is_default() {
                    None
                } else {
                    Some(self.inner().layout.root.clone())
                };
                let _ = self
                    .update_settings(move |s| s.system.data_dir = back)
                    .await;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Ask the daemon to drain and exit (the app re-attaches and a fresh daemon starts).
    pub fn request_restart(&self) {
        let tx = self.inner().restart.clone();
        // Let the RPC reply leave first.
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            let _ = tx.send(true);
        });
    }

    /// Resolves once a restart was requested (sr-daemon drains on it).
    pub async fn restart_requested(&self) {
        let mut rx = self.inner().restart.subscribe();
        let _ = rx.wait_for(|v| *v).await;
    }
}
