//! Folder policy: port of Selene's `resolveFolderSyncBehavior` / `shouldRunForTrigger`
//! (`lib/vectordb/sync-mode-resolver.ts`) plus the reindex-policy rules
//! (`sync-service.ts` / `sync-helpers.ts::shouldSmartReindex`).
//!
//! Selene semantics, kept exactly:
//!
//! | sync mode  | allows_watcher_events | allows_scheduled_runs | allows_automatic_add_sync |
//! |------------|-----------------------|-----------------------|---------------------------|
//! | auto       | yes                   | yes                   | yes                       |
//! | triggered  | yes                   | no                    | no                        |
//! | scheduled  | no                    | yes                   | no                        |
//! | manual     | no                    | no                    | no                        |
//!
//! * `keep_up_to_date = false` always means manual ([`FolderSettings::effective_sync_mode`]).
//! * Cadence is `max(cadence_min, 5)` minutes (Selene's floor; the schema bounds it to 5–1440).
//! * A manual run is always allowed (Selene: `trigger === "manual"`).
//! * Every folder's contents are searched: Selene's `files_only` indexing mode has no counterpart
//!   (the Files-only mode was removed, Amendment 17). Only access grants are listed without being
//!   read ([`crate::reconcile::behavior_of`]).
//!
//! Additions (documented deviations):
//! * **SMB roots** (`is_network_share`) get a scheduled reconcile instead of a watcher
//!   (docs/02-architecture.md §2.7). A watcher-only mode (`triggered`) on a share therefore still
//!   reconciles on its cadence, otherwise it would never see a change.
//! * **Startup / reconnect** runs are catch-up reconciles: allowed whenever the folder takes
//!   watcher events or scheduled runs (events missed while the daemon was down or the drive was
//!   unplugged). A settings change made by the user is always allowed (like manual).

use sr_catalog::RunTrigger;
use sr_common::config::{FolderSettings, ReindexPolicy, SyncMode};

/// Selene's minimum cadence (`MIN_SYNC_CADENCE_MINUTES`).
pub const MIN_CADENCE_MIN: u32 = 5;
/// Selene's `SMART_REINDEX_INTERVAL_MS`: how often a `smart` folder gets a deep verify.
pub const SMART_VERIFY_INTERVAL_MS: i64 = 24 * 60 * 60 * 1000;

/// Resolved sync behaviour of a folder (port of Selene's `resolveFolderSyncBehavior`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FolderBehavior {
    /// Run a file watcher (M2): watcher events allowed and the root is not a network share.
    pub watch: bool,
    /// Periodic reconcile cadence, `None` = never scheduled.
    pub reconcile_every_min: Option<u32>,
    /// Read and embed the files. Always `true` from settings; `false` only for access grants,
    /// whose files are listed, not read ([`crate::reconcile::behavior_of`]).
    pub embed: bool,
    /// The effective sync mode (`keep_up_to_date = false` → manual).
    pub sync_mode: SyncMode,
    /// Selene `allowsWatcherEvents`: `auto | triggered`.
    pub allows_watcher_events: bool,
    /// Selene `allowsScheduledRuns`: `auto | scheduled`.
    pub allows_scheduled_runs: bool,
    /// Selene `allowsAutomaticAddSync`: `auto` (index right after the folder is added).
    pub allows_automatic_add_sync: bool,
    pub reindex_policy: ReindexPolicy,
}

impl FolderBehavior {
    /// Derive the behaviour from folder settings. `is_network_share`: SMB roots use a scheduled
    /// reconcile instead of a watcher (docs/02-architecture.md §2.7).
    pub fn resolve(s: &FolderSettings, is_network_share: bool) -> Self {
        let sync_mode = s.effective_sync_mode();
        let allows_watcher_events = matches!(sync_mode, SyncMode::Auto | SyncMode::Triggered);
        let allows_scheduled_runs = matches!(sync_mode, SyncMode::Auto | SyncMode::Scheduled);
        let allows_automatic_add_sync = sync_mode == SyncMode::Auto;
        let cadence = s.cadence_min.max(MIN_CADENCE_MIN);
        let scheduled = allows_scheduled_runs || (allows_watcher_events && is_network_share);
        Self {
            watch: allows_watcher_events && !is_network_share,
            reconcile_every_min: scheduled.then_some(cadence),
            embed: true,
            sync_mode,
            allows_watcher_events,
            allows_scheduled_runs,
            allows_automatic_add_sync,
            reindex_policy: s.reindex_policy,
        }
    }

    /// Selene `shouldRunForTrigger`, extended to every catalog [`RunTrigger`] (module docs).
    pub fn should_run(&self, trigger: RunTrigger) -> bool {
        match trigger {
            RunTrigger::Manual | RunTrigger::Mcp | RunTrigger::SettingsChange => true,
            RunTrigger::Watcher => self.allows_watcher_events,
            RunTrigger::Schedule => self.reconcile_every_min.is_some(),
            RunTrigger::Startup | RunTrigger::Reconnect => {
                self.allows_watcher_events || self.reconcile_every_min.is_some()
            }
        }
    }

    /// Run right after the folder is added (Selene trigger `"auto"`).
    pub fn should_sync_on_add(&self) -> bool {
        self.allows_automatic_add_sync
    }

    /// How hard a scan checks file contents, for this trigger.
    ///
    /// * `always` re-indexes every included file on every run (Selene `forceReindex`).
    /// * `smart` on a scheduled run re-hashes every file once per [`SMART_VERIFY_INTERVAL_MS`]
    ///   (Selene's periodic smart reindex; with content hashes only real changes re-index).
    /// * otherwise (and for `never`, which in Selene only disables forcing): tiered detection
    ///   (size + mtime, then blake3 for changed or racy files).
    pub fn scan_depth(
        &self,
        trigger: RunTrigger,
        last_verify_at: Option<i64>,
        now: i64,
    ) -> ScanDepth {
        match self.reindex_policy {
            ReindexPolicy::Always => ScanDepth::ForceAll,
            ReindexPolicy::Smart
                if trigger == RunTrigger::Schedule
                    && last_verify_at.is_none_or(|t| now - t >= SMART_VERIFY_INTERVAL_MS) =>
            {
                ScanDepth::VerifyAll
            }
            ReindexPolicy::Smart | ReindexPolicy::Never => ScanDepth::Tiered,
        }
    }
}

/// Change-detection depth of one scan (see [`FolderBehavior::scan_depth`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanDepth {
    /// size + mtime, then blake3 only for changed or racy files.
    Tiered,
    /// blake3 for every file; re-index where the hash differs.
    VerifyAll,
    /// Re-index every included file.
    ForceAll,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(mode: SyncMode, keep: bool) -> FolderSettings {
        FolderSettings {
            sync_mode: mode,
            keep_up_to_date: keep,
            ..FolderSettings::default()
        }
    }

    /// Golden table: every `resolveFolderSyncBehavior` mode × share × trigger case.
    #[test]
    fn golden_behaviour_table() {
        use RunTrigger as T;
        // (mode, keep, share) -> (watcher, scheduled, add, watch, reconcile)
        let cases = [
            (
                SyncMode::Auto,
                true,
                false,
                (true, true, true, true, Some(60)),
            ),
            (
                SyncMode::Auto,
                true,
                true,
                (true, true, true, false, Some(60)),
            ),
            (
                SyncMode::Triggered,
                true,
                false,
                (true, false, false, true, None),
            ),
            (
                SyncMode::Triggered,
                true,
                true,
                (true, false, false, false, Some(60)),
            ),
            (
                SyncMode::Scheduled,
                true,
                false,
                (false, true, false, false, Some(60)),
            ),
            (
                SyncMode::Scheduled,
                true,
                true,
                (false, true, false, false, Some(60)),
            ),
            (
                SyncMode::Manual,
                true,
                false,
                (false, false, false, false, None),
            ),
            (
                SyncMode::Auto,
                false,
                false,
                (false, false, false, false, None),
            ),
            (
                SyncMode::Scheduled,
                false,
                true,
                (false, false, false, false, None),
            ),
        ];
        for (mode, keep, share, (w, s, a, watch, rec)) in cases {
            let b = FolderBehavior::resolve(&settings(mode, keep), share);
            let ctx = format!("{mode:?} keep={keep} share={share}");
            assert_eq!(b.allows_watcher_events, w, "{ctx}");
            assert_eq!(b.allows_scheduled_runs, s, "{ctx}");
            assert_eq!(b.allows_automatic_add_sync, a, "{ctx}");
            assert_eq!(b.should_sync_on_add(), a, "{ctx}");
            assert_eq!(b.watch, watch, "{ctx}");
            assert_eq!(b.reconcile_every_min, rec, "{ctx}");
            // Selene shouldRunForTrigger.
            assert!(b.should_run(T::Manual) && b.should_run(T::Mcp), "{ctx}");
            assert!(b.should_run(T::SettingsChange), "{ctx}");
            assert_eq!(b.should_run(T::Watcher), w, "{ctx}");
            assert_eq!(b.should_run(T::Schedule), rec.is_some(), "{ctx}");
            assert_eq!(b.should_run(T::Startup), w || rec.is_some(), "{ctx}");
            assert_eq!(b.should_run(T::Reconnect), w || rec.is_some(), "{ctx}");
        }
    }

    #[test]
    fn cadence_floor_and_contents_are_read() {
        let s = FolderSettings {
            cadence_min: 1,
            ..FolderSettings::default()
        };
        let b = FolderBehavior::resolve(&s, false);
        assert_eq!(b.reconcile_every_min, Some(MIN_CADENCE_MIN));
        assert!(
            b.embed,
            "no setting stops a folder's contents from being read"
        );
        let s = FolderSettings {
            cadence_min: 15,
            ..FolderSettings::default()
        };
        assert_eq!(
            FolderBehavior::resolve(&s, false).reconcile_every_min,
            Some(15),
            "cadence below 1 h is honoured (Selene bug)"
        );
    }

    #[test]
    fn scan_depth_by_reindex_policy() {
        let mk = |p| {
            FolderBehavior::resolve(
                &FolderSettings {
                    reindex_policy: p,
                    ..FolderSettings::default()
                },
                false,
            )
        };
        let now = 10 * SMART_VERIFY_INTERVAL_MS;
        let always = mk(ReindexPolicy::Always);
        assert_eq!(
            always.scan_depth(RunTrigger::Manual, None, now),
            ScanDepth::ForceAll
        );
        let smart = mk(ReindexPolicy::Smart);
        assert_eq!(
            smart.scan_depth(RunTrigger::Schedule, None, now),
            ScanDepth::VerifyAll
        );
        assert_eq!(
            smart.scan_depth(RunTrigger::Schedule, Some(now - 1000), now),
            ScanDepth::Tiered
        );
        assert_eq!(
            smart.scan_depth(
                RunTrigger::Schedule,
                Some(now - SMART_VERIFY_INTERVAL_MS),
                now
            ),
            ScanDepth::VerifyAll
        );
        assert_eq!(
            smart.scan_depth(RunTrigger::Manual, None, now),
            ScanDepth::Tiered
        );
        let never = mk(ReindexPolicy::Never);
        assert_eq!(
            never.scan_depth(RunTrigger::Schedule, None, now),
            ScanDepth::Tiered
        );
    }

    #[test]
    fn default_folder_watches_and_reconciles_hourly() {
        let b = FolderBehavior::resolve(&FolderSettings::default(), false);
        assert!(b.watch && b.embed);
        assert_eq!(b.reconcile_every_min, Some(60));
        assert!(!FolderBehavior::resolve(&FolderSettings::default(), true).watch);
        let manual = FolderSettings {
            keep_up_to_date: false,
            ..FolderSettings::default()
        };
        assert_eq!(
            FolderBehavior::resolve(&manual, false).reconcile_every_min,
            None
        );
    }
}
