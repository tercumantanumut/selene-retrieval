//! R34 (M5-E): the moved search index is missing at start (drive unplugged, folder renamed or
//! deleted).
//!
//! The service never starts on an empty index in its place (that would show "No folders yet" and
//! later fork the user's index). Instead `sr-daemon` opens the pipe in a **recovery mode**: the
//! status says "Search index not found" with [`IndexMissingInfo`], every other call answers
//! `IndexMissing` ("Reconnect the drive, or move the index back"), the service restarts by itself
//! as soon as the folder is back, and the app offers two choices (`index_location.recover`):
//!
//! * **Move it back** ([`RecoverChoice::MoveBack`]): only when a complete earlier copy still
//!   exists (usually the default place, kept because its cleanup never ran). The pointer goes back
//!   to it; the normal re-scan picks up changes made since.
//! * **Start fresh** ([`RecoverChoice::StartFresh`]): an empty index in the default place. The
//!   folders are added back from the service's folder-list backup ([`FOLDERS_BACKUP`], kept in
//!   the anchor next to `settings.toml`, which never moves) and indexed again. The missing copy is
//!   not touched; if that drive returns later it simply isn't used.
//!
//! Nothing is ever deleted from the missing location, and the user's folders are never touched.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sr_catalog::model::FolderKind;
use sr_catalog::repo::folders;
use sr_common::api::{IndexMissingInfo, RecoverChoice};
use sr_common::config::FolderSettings;
use sr_common::error::{Error, ErrorCode};

use crate::EngineHandle;
use crate::datadir::{self, INDEX_ENTRIES};
use crate::indexmove;

/// The folder list the service keeps in the anchor (rewritten when folders change).
pub const FOLDERS_BACKUP: &str = "folders-backup.json";
/// Written by "Start fresh": the next start adds these folders back to the empty index.
pub const RESTORE_FOLDERS: &str = "restore-folders.json";

/// One remembered folder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackupFolder {
    pub path: PathBuf,
    pub settings: FolderSettings,
}

/// `folders-backup.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FolderBackup {
    pub version: u32,
    pub folders: Vec<BackupFolder>,
}

fn read_backup(path: &Path) -> Option<FolderBackup> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Whether the index root holds an index (its catalog file).
fn has_index(root: &Path) -> bool {
    root.join("catalog.db").is_file()
}

fn drive_of(p: &Path) -> Option<String> {
    match p.components().next()? {
        std::path::Component::Prefix(px) => match px.kind() {
            std::path::Prefix::Disk(d) | std::path::Prefix::VerbatimDisk(d) => {
                Some(format!("{}:", char::from(d).to_ascii_uppercase()))
            }
            _ => None,
        },
        _ => None,
    }
}

fn same(a: &Path, b: &Path) -> bool {
    sr_client::naming::data_dir_key(a) == sr_client::naming::data_dir_key(b)
}

/// A complete earlier copy "Move it back" can use: the anchor (default place) or the move
/// plan's source, if either still holds an index.
fn old_copy(anchor: &Path, missing: &Path) -> Option<PathBuf> {
    let mut candidates = vec![anchor.to_path_buf()];
    if let Some(plan) = indexmove::read_plan(anchor) {
        candidates.push(plan.from);
    }
    candidates
        .into_iter()
        .find(|c| !same(c, missing) && datadir::validate_index_root(c).is_ok() && has_index(c))
}

/// The moved index is missing: the pointer is valid by the path rule but its folder is gone.
/// `None` when the index is where it should be (or was never moved).
pub fn missing_index(anchor: &Path) -> Option<IndexMissingInfo> {
    let p = datadir::read_pointer(anchor)?;
    if datadir::validate_index_root(&p).is_err() || p.is_dir() {
        return None;
    }
    let restore = read_backup(&anchor.join(FOLDERS_BACKUP)).map_or(0, |b| b.folders.len());
    Some(IndexMissingInfo {
        path: p.display().to_string(),
        drive: drive_of(&p),
        old_copy: old_copy(anchor, &p).map(|c| c.display().to_string()),
        folders_to_restore: u32::try_from(restore).unwrap_or(u32::MAX),
    })
}

/// The `IndexMissing` error for a missing index at `path`.
pub fn missing_error(info: &IndexMissingInfo) -> Error {
    Error::new(ErrorCode::IndexMissing)
        .with_name(info.path.clone())
        .with_detail("the moved search index folder is missing")
}

/// `[system] data_dir` mirrors the index place for display (the pointer file is the truth).
fn set_settings_data_dir(anchor: &Path, shown: Option<PathBuf>) {
    let path = anchor.join(sr_common::config::SETTINGS_FILE);
    let mut r = sr_common::config::load_or_create(&path);
    if r.settings.system.data_dir != shown {
        r.settings.system.data_dir = shown;
        if let Err(e) = sr_common::config::save_atomic(&path, &r.settings) {
            tracing::warn!(target: "sr_engine::recover", code = %e.code, "settings not updated after recovering the index");
        }
    }
}

/// Carry out a recovery choice (blocking; the service then restarts on the result).
pub fn recover(anchor: &Path, choice: RecoverChoice) -> Result<PathBuf, Error> {
    let Some(info) = missing_index(anchor) else {
        return Err(Error::new(ErrorCode::InvalidInput)
            .with_detail("the search index is in place; there is nothing to recover"));
    };
    let missing = PathBuf::from(&info.path);
    match choice {
        RecoverChoice::MoveBack => {
            let Some(old) = old_copy(anchor, &missing) else {
                return Err(Error::new(ErrorCode::NotFound)
                    .with_detail("no earlier copy of the search index is left to move back to"));
            };
            if same(&old, anchor) {
                datadir::write_pointer(anchor, None)?;
                set_settings_data_dir(anchor, None);
            } else {
                datadir::write_pointer(anchor, Some(&old))?;
                set_settings_data_dir(anchor, Some(old.clone()));
            }
            indexmove::remove_plan(anchor);
            tracing::info!(target: "sr_engine::recover", "search index moved back to an earlier copy");
            Ok(old)
        }
        RecoverChoice::StartFresh => {
            // An older copy in the default place would otherwise be picked up as is. Downloaded
            // models are kept (they don't depend on the index).
            for e in INDEX_ENTRIES.iter().filter(|e| **e != "models") {
                let p = anchor.join(e);
                let r = if p.is_dir() {
                    std::fs::remove_dir_all(&p)
                } else {
                    std::fs::remove_file(&p)
                };
                if let Err(err) = r
                    && err.kind() != std::io::ErrorKind::NotFound
                {
                    return Err(Error::from(err).with_detail(format!(
                        "clearing the old index entry {e} for a fresh start"
                    )));
                }
            }
            match std::fs::copy(anchor.join(FOLDERS_BACKUP), anchor.join(RESTORE_FOLDERS)) {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(
                        Error::from(e).with_detail("keeping the folder list for a fresh start")
                    );
                }
            }
            datadir::write_pointer(anchor, None)?;
            set_settings_data_dir(anchor, None);
            indexmove::remove_plan(anchor);
            tracing::info!(target: "sr_engine::recover", folders = info.folders_to_restore, "starting a fresh search index in the default place");
            Ok(anchor.to_path_buf())
        }
    }
}

impl EngineHandle {
    /// Rewrite the anchor's folder-list backup when the folders changed (start, add, remove,
    /// settings change). Best effort: a failure is logged and never fails the caller.
    pub(crate) async fn write_folder_backup(&self) {
        let anchor = self.inner().layout.anchor.clone();
        let rows = match self.inner().cat.read_async(folders::folders).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(target: "sr_engine::recover", code = %e.code, "folder list backup not written");
                return;
            }
        };
        let backup = FolderBackup {
            version: 1,
            folders: rows
                .into_iter()
                .filter(|f| f.kind == FolderKind::Indexed)
                .map(|f| BackupFolder {
                    path: f.root_path,
                    settings: f.settings,
                })
                .collect(),
        };
        let r = tokio::task::spawn_blocking(move || {
            let path = anchor.join(FOLDERS_BACKUP);
            if read_backup(&path).as_ref() == Some(&backup) {
                return Ok(());
            }
            let body = serde_json::to_vec_pretty(&backup)
                .map_err(|e| Error::new(ErrorCode::Internal).with_source(e))?;
            let tmp = anchor.join(format!("{FOLDERS_BACKUP}.tmp"));
            datadir::write_synced(&tmp, &body)?;
            std::fs::rename(&tmp, &path).map_err(Error::from)
        })
        .await;
        if let Ok(Err(e)) = r {
            tracing::warn!(target: "sr_engine::recover", code = %e.code, "folder list backup not written");
        }
    }

    /// After "Start fresh": add the remembered folders back to the empty index (each through the
    /// normal checks: dangerous roots, policy, overlap) and restore their settings. Runs once;
    /// the marker is removed afterwards. Returns how many folders were added.
    pub(crate) async fn restore_folders_if_requested(&self) -> usize {
        let marker = self.inner().layout.anchor.join(RESTORE_FOLDERS);
        let Some(backup) = read_backup(&marker) else {
            return 0;
        };
        let empty = self
            .inner()
            .cat
            .read_async(|c| Ok(folders::folder_ids(c)?.is_empty()))
            .await
            .unwrap_or(false);
        let mut added = 0;
        if empty {
            for f in backup.folders {
                let name = Some(f.settings.display_name.clone());
                match self.add_folder(&f.path.to_string_lossy(), name).await {
                    Ok(info) => {
                        added += 1;
                        if let Err(e) = self
                            .update_folder_settings(info.folder_id, f.settings)
                            .await
                        {
                            tracing::warn!(target: "sr_engine::recover", code = %e.code, "folder added back with default settings");
                        }
                    }
                    Err(e) => {
                        tracing::warn!(target: "sr_engine::recover", code = %e.code, "a remembered folder could not be added back");
                    }
                }
            }
            tracing::info!(target: "sr_engine::recover", added, "folders added back after a fresh start");
        }
        let _ = std::fs::remove_file(&marker);
        added
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    fn seed(root: &Path) {
        std::fs::create_dir_all(root.join("lance")).unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(root.join("catalog.db"), "x").unwrap_or_else(|e| panic!("{e}"));
    }

    fn backup(anchor: &Path, n: usize) {
        let b = FolderBackup {
            version: 1,
            folders: (0..n)
                .map(|i| BackupFolder {
                    path: PathBuf::from(format!(r"C:\data\f{i}")),
                    settings: FolderSettings::default(),
                })
                .collect(),
        };
        std::fs::write(
            anchor.join(FOLDERS_BACKUP),
            serde_json::to_vec(&b).unwrap_or_default(),
        )
        .unwrap_or_else(|e| panic!("{e}"));
    }

    #[test]
    fn missing_index_is_detected_and_explained() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let anchor = dir.path().join("a");
        std::fs::create_dir_all(&anchor).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(missing_index(&anchor), None, "never moved");
        let moved = dir.path().join("gone");
        datadir::write_pointer(&anchor, Some(&moved)).unwrap_or_else(|e| panic!("{e}"));
        backup(&anchor, 2);
        let info = missing_index(&anchor).unwrap_or_else(|| panic!("missing"));
        assert_eq!(info.folders_to_restore, 2);
        assert_eq!(info.old_copy, None, "the default place holds no index");
        // The start refuses with the dedicated code, not the added-folder sentence.
        let e = datadir::prepare(&anchor)
            .err()
            .unwrap_or_else(|| panic!("start"));
        assert_eq!(e.code, ErrorCode::IndexMissing);
        assert!(!e.user_message().agent_text().contains("add the folder"));
        // The drive comes back: nothing to recover.
        std::fs::create_dir_all(&moved).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(missing_index(&anchor), None);
    }

    #[test]
    fn move_back_uses_the_old_copy() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let anchor = dir.path().join("a");
        seed(&anchor);
        let moved = dir.path().join("gone");
        datadir::write_pointer(&anchor, Some(&moved)).unwrap_or_else(|e| panic!("{e}"));
        let info = missing_index(&anchor).unwrap_or_else(|| panic!("missing"));
        assert_eq!(info.old_copy, Some(anchor.display().to_string()));
        let to = recover(&anchor, RecoverChoice::MoveBack).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(to, anchor);
        assert_eq!(datadir::read_pointer(&anchor), None);
        assert!(has_index(&anchor), "the old copy is used as it is");
        assert!(datadir::prepare(&anchor).is_ok());
        // Nothing left to recover now.
        assert_eq!(
            recover(&anchor, RecoverChoice::MoveBack)
                .err()
                .map(|e| e.code),
            Some(ErrorCode::InvalidInput)
        );
    }

    #[test]
    fn start_fresh_clears_the_default_place_and_remembers_folders() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let anchor = dir.path().join("a");
        seed(&anchor);
        backup(&anchor, 3);
        let moved = dir.path().join("gone");
        datadir::write_pointer(&anchor, Some(&moved)).unwrap_or_else(|e| panic!("{e}"));
        recover(&anchor, RecoverChoice::StartFresh).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(datadir::read_pointer(&anchor), None);
        assert!(!has_index(&anchor) && !anchor.join("lance").exists());
        let r = read_backup(&anchor.join(RESTORE_FOLDERS)).unwrap_or_default();
        assert_eq!(r.folders.len(), 3);
        // The settings file is kept (it lives in the anchor and never moves).
        assert!(datadir::prepare(&anchor).is_ok());
    }

    #[test]
    fn move_back_without_a_copy_is_refused_plainly() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let anchor = dir.path().join("a");
        std::fs::create_dir_all(&anchor).unwrap_or_else(|e| panic!("{e}"));
        datadir::write_pointer(&anchor, Some(&dir.path().join("gone")))
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            recover(&anchor, RecoverChoice::MoveBack)
                .err()
                .map(|e| e.code),
            Some(ErrorCode::NotFound)
        );
        assert!(datadir::read_pointer(&anchor).is_some(), "nothing changed");
    }
}
