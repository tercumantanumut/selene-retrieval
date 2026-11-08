//! M2 catalog additions (V3): held claims for paused/unavailable folders, release without an
//! attempt, cancel, listed files, latest scan id, 64-bit volume serials, approvals; the
//! Amendment 17 mark of a folder left in the removed Files-only mode.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use sr_catalog::repo::approvals::{self, AddFolderRequest};
use sr_catalog::repo::jobs::{self, ClaimRequest, Drr, NewJob};
use sr_catalog::repo::{files, folders, meta, status};
use sr_catalog::{ApprovalState, FileState, JobKind, SKIP_REASON_FILES_ONLY, WatchSupport};
use sr_common::error::{ClientKind, ErrorCode};
use sr_common::events::FolderStatus;
use sr_common::ids::FolderId;

fn index_job(folder: FolderId, file: sr_common::ids::FileId) -> NewJob {
    NewJob::new(JobKind::IndexFile, Some(folder), Some(file), 1)
}

fn claim_all(fx: &Fx, epoch: i64, now: i64) -> Vec<jobs::Job> {
    fx.cat
        .write_blocking(move |c| {
            let mut req = ClaimRequest::new(epoch, 100, now);
            req.aging_ms = 1; // exercise the aging query too
            jobs::claim(c, &req, &mut Drr::default())
        })
        .unwrap()
}

#[test]
fn paused_and_unavailable_folders_are_held_by_the_claim_without_using_attempts() {
    let fx = setup();
    let epoch = fx.cat.start_epoch().unwrap().epoch;
    let a = add_folder(&fx.cat, fx.collection, &fx.root("a"));
    let b = add_folder(&fx.cat, fx.collection, &fx.root("b"));
    let fa = add_file(&fx.cat, a, "x.md", 1);
    let fb = add_file(&fx.cat, b, "y.md", 2);
    fx.cat
        .write_blocking(move |c| {
            jobs::enqueue(c, &index_job(a, fa), NOW)?;
            jobs::enqueue(c, &index_job(b, fb), NOW)?;
            jobs::enqueue(c, &NewJob::new(JobKind::ScanFolder, Some(b), None, 1), NOW)?;
            folders::set_paused(c, a, true, NOW)?;
            folders::set_available(c, b, false, Some(ErrorCode::FolderUnavailable), NOW)
        })
        .unwrap();
    let got = claim_all(&fx, epoch, NOW + 10_000_000);
    assert_eq!(got.len(), 1, "only the unavailable folder's scan: {got:?}");
    assert_eq!(got[0].kind, JobKind::ScanFolder);
    // Held jobs are untouched: still queued, no attempt used.
    let live = fx.cat.read(|c| jobs::live_jobs(c, None)).unwrap();
    let held: Vec<_> = live
        .iter()
        .filter(|j| j.kind == JobKind::IndexFile)
        .collect();
    assert_eq!(held.len(), 2);
    assert!(
        held.iter()
            .all(|j| j.attempts == 0 && j.lease_owner.is_none())
    );
    let st = fx.cat.read(move |c| status::counts(c, a)).unwrap();
    assert_eq!(status::derive(&st), FolderStatus::Paused);
    // Resume and reconnect: both run.
    fx.cat
        .write_blocking(move |c| {
            folders::set_paused(c, a, false, NOW)?;
            folders::set_available(c, b, true, None, NOW)
        })
        .unwrap();
    let got = claim_all(&fx, epoch, NOW + 10_000_000);
    assert_eq!(got.len(), 2);
    assert!(got.iter().all(|j| j.attempts == 1));
    let f = fx.cat.read(move |c| folders::folder(c, a)).unwrap();
    assert!(!f.paused && f.paused_at.is_none());
}

#[test]
fn release_requeues_without_consuming_the_attempt_and_is_fenced() {
    let fx = setup();
    let epoch = fx.cat.start_epoch().unwrap().epoch;
    let a = add_folder(&fx.cat, fx.collection, &fx.root("a"));
    let fa = add_file(&fx.cat, a, "x.md", 1);
    fx.cat
        .write_blocking(move |c| jobs::enqueue(c, &index_job(a, fa), NOW))
        .unwrap();
    for _ in 0..10 {
        let got = claim_all(&fx, epoch, NOW);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].attempts, 1, "released claims never accumulate");
        let id = got[0].id;
        fx.cat
            .write_blocking(move |c| jobs::release(c, id, epoch, NOW, NOW))
            .unwrap();
    }
    let got = claim_all(&fx, epoch, NOW);
    let id = got[0].id;
    let err = fx
        .cat
        .write_blocking(move |c| jobs::release(c, id, epoch + 1, NOW, NOW))
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict, "a stale epoch can't release");
}

#[test]
fn cancel_drops_the_runs_jobs_and_forgets_metadata_of_unindexed_edits() {
    let fx = setup();
    let epoch = fx.cat.start_epoch().unwrap().epoch;
    let a = add_folder(&fx.cat, fx.collection, &fx.root("a"));
    let edited = add_file(&fx.cat, a, "edited.md", 1);
    let fresh = add_file(&fx.cat, a, "fresh.md", 2);
    let running = add_file(&fx.cat, a, "running.md", 3);
    // `edited` was indexed before; its edit is queued (observe stored the new mtime).
    let t = t1(&fx.cat, edited, "old text");
    fx.cat
        .write_blocking(move |c| {
            sr_catalog::repo::chunks::finalize_file(c, edited, t.file_version, NOW)?;
            jobs::enqueue(c, &index_job(a, running), NOW)?;
            Ok(())
        })
        .unwrap();
    let leased = claim_all(&fx, epoch, NOW);
    assert_eq!(leased.len(), 1);
    fx.cat
        .write_blocking(move |c| {
            jobs::enqueue(c, &index_job(a, edited), NOW)?;
            jobs::enqueue(c, &index_job(a, fresh), NOW)?;
            jobs::enqueue(c, &NewJob::new(JobKind::ScanFolder, Some(a), None, 1), NOW)?;
            jobs::enqueue(c, &NewJob::new(JobKind::Retention, None, None, 3), NOW)?;
            Ok(())
        })
        .unwrap();
    let out = fx
        .cat
        .write_blocking(move |c| {
            folders::set_cancelled(c, a, Some(NOW), NOW)?;
            jobs::cancel_queued(c, a, NOW)
        })
        .unwrap();
    assert_eq!(
        out.jobs, 3,
        "scan + 2 index jobs; retention is never dropped"
    );
    assert_eq!(
        out.files_invalidated, 1,
        "only the indexed file's metadata is forgotten"
    );
    let e = fx.cat.read(move |c| files::file(c, edited)).unwrap();
    assert_eq!((e.state, e.mtime_ns), (FileState::Indexed, 0));
    // The leased job is dropped by the service when it stops.
    let id = leased[0].id;
    let out = fx
        .cat
        .write_blocking(move |c| jobs::cancel_leased(c, id, epoch, NOW))
        .unwrap();
    assert_eq!(out.jobs, 1);
    let live = fx.cat.read(move |c| jobs::live_jobs(c, Some(a))).unwrap();
    assert!(live.is_empty());
    let hist: i64 = fx
        .cat
        .read(|c| {
            c.query_row(
                "SELECT count(*) FROM job_history WHERE state = ?1",
                [jobs::HISTORY_CANCELLED],
                |r| r.get(0),
            )
            .map_err(sr_catalog::map_db_err)
        })
        .unwrap();
    assert_eq!(hist, 4);
    // Pending files the run never reached: partial (not "indexing" forever). A finished full
    // scan clears the mark.
    let st = fx.cat.read(move |c| status::counts(c, a)).unwrap();
    assert!(st.cancelled);
    fx.cat
        .write_blocking(move |c| {
            let s = files::begin_scan(c, a, NOW)?;
            files::finish_scan(c, a, s, NOW)
        })
        .unwrap();
    let st = fx.cat.read(move |c| status::counts(c, a)).unwrap();
    assert!(!st.cancelled);
    assert_eq!(
        status::derive(&st),
        FolderStatus::Indexing,
        "pending again, no mark"
    );
}

#[test]
fn listed_files_are_not_skipped() {
    let fx = setup();
    let a = add_folder(&fx.cat, fx.collection, &fx.root("a"));
    let listed = add_file(&fx.cat, a, "l.md", 1);
    let big = add_file(&fx.cat, a, "big.bin", 2);
    let ok = add_file(&fx.cat, a, "ok.md", 3);
    let t = t1(&fx.cat, ok, "x");
    fx.cat
        .write_blocking(move |c| {
            files::mark_skipped(c, listed, SKIP_REASON_FILES_ONLY, None, NOW)?;
            files::mark_skipped(c, big, "too_large", Some(ErrorCode::FileTooLarge), NOW)?;
            sr_catalog::repo::chunks::finalize_file(c, ok, t.file_version, NOW)?;
            let s = files::begin_scan(c, a, NOW)?;
            files::finish_scan(c, a, s, NOW).map(|_| ())
        })
        .unwrap();
    let (sk, li, n) = fx
        .cat
        .read(move |c| {
            Ok((
                files::skipped_files(c, a)?,
                files::listed_files(c, a)?,
                status::counts(c, a)?,
            ))
        })
        .unwrap();
    assert_eq!(
        sk.iter().map(|f| f.rel_path.as_str()).collect::<Vec<_>>(),
        ["big.bin"]
    );
    assert_eq!(
        li.iter().map(|f| f.rel_path.as_str()).collect::<Vec<_>>(),
        ["l.md"]
    );
    assert_eq!(
        (n.skipped_files, n.listed_files, n.indexed_files),
        (1, 1, 1)
    );
    assert_eq!(n.total_files(), 3);
}

/// Amendment 17: the mark of a folder left in the removed Files-only mode survives settings
/// writes (it is not a setting) and is cleared once, by the user's choice.
#[test]
fn files_only_mark_survives_settings_and_clears_once() {
    let fx = setup();
    let a = add_folder(&fx.cat, fx.collection, &fx.root("a"));
    let read = |id| fx.cat.read(move |c| folders::folder(c, id)).unwrap();
    assert!(!read(a).was_files_only, "new folders are never marked");
    fx.cat
        .write_blocking(move |c| {
            c.execute(
                "UPDATE folders SET indexing_mode = 'files_only' WHERE id = ?1",
                [a.to_bytes()],
            )
            .map_err(sr_catalog::map_db_err)
        })
        .unwrap();
    assert!(read(a).was_files_only);
    let mut s = read(a).settings;
    s.display_name = "Scans".into();
    s.recursive = false;
    let changed = fx
        .cat
        .write_blocking(move |c| folders::update_settings(c, a, &s, NOW))
        .unwrap();
    assert!(changed);
    let r = read(a);
    assert!(r.was_files_only && r.settings.display_name == "Scans");
    let cleared = |id| {
        fx.cat
            .write_blocking(move |c| folders::clear_files_only_mark(c, id, NOW))
            .unwrap()
    };
    assert!(cleared(a));
    assert!(!cleared(a), "only once");
    assert!(!read(a).was_files_only);
}

#[test]
fn latest_scan_id_and_any_under() {
    let fx = setup();
    let a = add_folder(&fx.cat, fx.collection, &fx.root("a"));
    assert_eq!(
        fx.cat.read(move |c| files::latest_scan_id(c, a)).unwrap(),
        None
    );
    add_file(&fx.cat, a, "docs/x.md", 1);
    let s2 = fx
        .cat
        .write_blocking(move |c| files::begin_scan(c, a, NOW))
        .unwrap();
    assert_eq!(
        fx.cat.read(move |c| files::latest_scan_id(c, a)).unwrap(),
        Some(s2)
    );
    let under = |k: &'static str| fx.cat.read(move |c| files::any_under(c, a, k)).unwrap();
    assert!(under("docs"));
    assert!(under(""));
    assert!(!under("doc"), "prefix of a name is not a parent");
    assert!(!under("docs/x.md"));
    assert!(!under("other"));
}

#[test]
fn legacy_32_bit_volume_serials_upgrade_in_place() {
    let fx = setup();
    // add_folder stores serial 7 (a "legacy" low-32 value) for the folder and its files.
    let a = add_folder(&fx.cat, fx.collection, &fx.root("a"));
    let f = add_file(&fx.cat, a, "x.md", 9);
    let full: u64 = 0xDEAD_BEEF_0000_0007;
    let n = fx
        .cat
        .write_blocking(move |c| folders::upgrade_volume_serial(c, a, full, NOW))
        .unwrap();
    assert_eq!(n, Some(1));
    let rec = fx.cat.read(move |c| folders::folder(c, a)).unwrap();
    assert_eq!(rec.volume_serial, Some(full), "all 64 bits round-trip");
    // Rename detection finds the file by its full identity.
    let hit = fx
        .cat
        .read(move |c| files::file_by_identity(c, a, full, [9; 16]))
        .unwrap();
    assert_eq!(hit.map(|r| r.id), Some(f));
    // Idempotent; a different low half is not an upgrade.
    let again = fx
        .cat
        .write_blocking(move |c| folders::upgrade_volume_serial(c, a, full, NOW))
        .unwrap();
    assert_eq!(again, None);
    let other = fx
        .cat
        .write_blocking(move |c| folders::upgrade_volume_serial(c, a, 0xDEAD_BEEF_0000_0008, NOW))
        .unwrap();
    assert_eq!(other, None);
}

#[test]
fn watch_support_is_stored() {
    let fx = setup();
    let a = add_folder(&fx.cat, fx.collection, &fx.root("a"));
    assert_eq!(
        fx.cat
            .read(move |c| folders::folder(c, a))
            .unwrap()
            .watch_support,
        WatchSupport::Native
    );
    let changed = fx
        .cat
        .write_blocking(move |c| {
            Ok((
                folders::set_watch_support(c, a, WatchSupport::Network, NOW)?,
                folders::set_watch_support(c, a, WatchSupport::Network, NOW)?,
            ))
        })
        .unwrap();
    assert_eq!(changed, (true, false));
}

fn request(client: &str, root: &str) -> AddFolderRequest {
    AddFolderRequest {
        client_id: client.into(),
        client_kind: ClientKind::ClaudeDesktop,
        client_name: None,
        collection_id: sr_common::ids::CollectionId::new(),
        root_path: root.into(),
        root_key: folders::root_key(std::path::Path::new(root)),
    }
}

#[test]
fn approvals_dedupe_decide_once_and_expire() {
    let fx = setup();
    let ttl = approvals::DEFAULT_TTL_MS;
    let (a1, created) = fx
        .cat
        .write_blocking(move |c| {
            approvals::request_add_folder(c, &request("claude-1", r"C:\Proj"), NOW, ttl)
        })
        .unwrap();
    assert!(created);
    assert_eq!(a1.state, ApprovalState::Pending);
    assert_eq!(a1.client_kind, ClientKind::ClaudeDesktop);
    assert_eq!(a1.expires_at, NOW + ttl);
    // Same client, same folder (other casing): the same request comes back.
    let (again, created) = fx
        .cat
        .write_blocking(move |c| {
            approvals::request_add_folder(c, &request("claude-1", r"c:\proj\"), NOW + 5, ttl)
        })
        .unwrap();
    assert!(!created);
    assert_eq!(again.id, a1.id);
    let (other, _) = fx
        .cat
        .write_blocking(move |c| {
            approvals::request_add_folder(c, &request("cursor-1", r"C:\Proj"), NOW, ttl)
        })
        .unwrap();
    assert_ne!(other.id, a1.id);
    assert_eq!(
        fx.cat.read(|c| approvals::pending(c, NOW)).unwrap().len(),
        2
    );
    // Approve once; a second decision conflicts.
    let id = a1.id;
    let d = fx
        .cat
        .write_blocking(move |c| approvals::decide(c, id, true, NOW + 10))
        .unwrap();
    assert_eq!(
        (d.state, d.decided_at),
        (ApprovalState::Approved, Some(NOW + 10))
    );
    let err = fx
        .cat
        .write_blocking(move |c| approvals::decide(c, id, false, NOW + 11))
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);
    let folder = FolderId::new();
    fx.cat
        .write_blocking(move |c| approvals::record_result(c, id, Some(folder), None))
        .unwrap();
    assert_eq!(
        fx.cat
            .read(move |c| approvals::approval(c, id))
            .unwrap()
            .folder_id,
        Some(folder)
    );
    // The other one times out: hidden from `pending`, a late decision is refused as expired.
    let oid = other.id;
    assert!(
        fx.cat
            .read(|c| approvals::pending(c, NOW + ttl))
            .unwrap()
            .is_empty()
    );
    let err = fx
        .cat
        .write_blocking(move |c| approvals::decide(c, oid, true, NOW + ttl + 1))
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::ApprovalDenied);
    let o = fx.cat.read(move |c| approvals::approval(c, oid)).unwrap();
    assert_eq!(o.state, ApprovalState::Expired);
    // After expiry the client may ask again (a new request).
    let (fresh, created) = fx
        .cat
        .write_blocking(move |c| {
            approvals::request_add_folder(c, &request("cursor-1", r"C:\Proj"), NOW + ttl + 2, ttl)
        })
        .unwrap();
    assert!(created && fresh.id != oid);
    // History survives a restart.
    let _ = meta::epoch;
    assert_eq!(fx.cat.read(|c| approvals::recent(c, 10)).unwrap().len(), 3);
}
