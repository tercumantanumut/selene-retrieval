//! M2 live sync: the notify watcher end to end (real ReadDirectoryChangesW), overflow and storm
//! handling, root loss, pause/resume/cancel through the claim loop, `add_folder` approvals,
//! network (reconcile-only) folders, and the 64-bit volume-serial upgrade.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use common::*;
use sr_catalog::repo::{approvals, files, folders, status};
use sr_catalog::{ApprovalState, FileState, JobKind, RunTrigger, WatchSupport};
use sr_common::config::{FolderSettings, SyncMode};
use sr_common::error::{ClientKind, ErrorCode};
use sr_common::events::{EngineEvent, FolderStatus};
use sr_common::ids::FolderId;
use sr_sync::{McpAddFolder, PathEvent, SyncService};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

type Svc = SyncService<FakeExec>;

fn file_id(fx: &Fx, folder: FolderId, rel: &str) -> Option<sr_common::ids::FileId> {
    let key = sr_common::sandbox::path_key(rel);
    fx.cat
        .read(move |c| files::file_by_path(c, folder, &key))
        .unwrap()
        .map(|r| r.id)
}

fn live_job_for(fx: &Fx, folder: FolderId, rel: &str, kind: JobKind) -> Option<sr_catalog::Job> {
    let id = file_id(fx, folder, rel)?;
    fx.live_jobs(Some(folder))
        .into_iter()
        .find(|j| j.kind == kind && j.file_id == Some(id))
}

fn live_scan(fx: &Fx, folder: FolderId) -> Option<sr_catalog::Job> {
    fx.live_jobs(Some(folder))
        .into_iter()
        .find(|j| j.kind == JobKind::ScanFolder)
}

/// Folder with `files`, scanned and indexed (baseline), plus a service. No loops running.
fn baseline(fx: &Fx, name: &str, files: &[(&str, &str)]) -> (Svc, FolderId, std::path::PathBuf) {
    let root = fx.dir(name);
    for (rel, body) in files {
        write(&root, rel, body);
    }
    let exec = Arc::new(FakeExec::new(&fx.cat));
    let svc = fx.service(exec, fx.epoch(), fast_cfg());
    let id = add(svc.reconciler(), fx, &root);
    svc.reconciler()
        .scan_blocking(id, RunTrigger::Manual, &CancellationToken::new())
        .unwrap();
    index_all_now(fx, id);
    (svc, id, root)
}

fn start_loop(
    svc: &Svc,
) -> (
    CancellationToken,
    tokio::task::JoinHandle<sr_common::Result<()>>,
) {
    let cancel = CancellationToken::new();
    let s = svc.clone();
    let c = cancel.clone();
    (cancel, tokio::spawn(async move { s.run(c).await }))
}

// ------------------------------------------------------------------------- real watcher

/// Exit criterion "save → searchable within 3 s": the sync layer's share (event → job) must be
/// well under a second. A new file and an edit of an indexed file each get their job < 1 s.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn save_event_becomes_a_job_within_one_second() {
    let fx = setup();
    let (svc, id, root) = baseline(&fx, "live", &[("a.md", "alpha")]);
    let cancel = CancellationToken::new();
    let w = svc.start_watcher(cancel.clone());
    wait_for(Duration::from_secs(10), || w.is_watching(id)).await;
    let mut worst = Duration::ZERO;
    for i in 0..5 {
        let rel = format!("note{i}.md");
        let t0 = Instant::now();
        write(&root, &rel, &format!("content number {i}"));
        wait_for(Duration::from_secs(5), || {
            live_job_for(&fx, id, &rel, JobKind::IndexFile).is_some()
        })
        .await;
        worst = worst.max(t0.elapsed());
    }
    let t0 = Instant::now();
    write(&root, "a.md", "alpha, edited to something longer");
    wait_for(Duration::from_secs(5), || {
        live_job_for(&fx, id, "a.md", JobKind::IndexFile).is_some()
    })
    .await;
    worst = worst.max(t0.elapsed());
    eprintln!("save -> job, worst of 6: {worst:?}");
    assert!(worst < Duration::from_secs(1), "save -> job took {worst:?}");
    assert!(w.counters().raw_events > 0 && w.counters().batches > 0);
    cancel.cancel();
    w.join().await;
}

/// Amendment 4.17 via the watcher: a rename keeps `file_id` and queues a path-only refresh.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rename_via_watcher_keeps_identity_and_queues_a_path_refresh() {
    let fx = setup();
    let (svc, id, root) = baseline(
        &fx,
        "ren",
        &[("old name.md", "quokka"), ("sub/keep.md", "k")],
    );
    let before = file_id(&fx, id, "old name.md").unwrap();
    let cancel = CancellationToken::new();
    let w = svc.start_watcher(cancel.clone());
    wait_for(Duration::from_secs(10), || w.is_watching(id)).await;
    std::fs::rename(
        root.join("old name.md"),
        root.join("sub").join("new name.md"),
    )
    .unwrap();
    wait_for(Duration::from_secs(5), || {
        live_job_for(&fx, id, "sub/new name.md", JobKind::IndexFile).is_some()
    })
    .await;
    let job = live_job_for(&fx, id, "sub/new name.md", JobKind::IndexFile).unwrap();
    assert_eq!(job.file_id, Some(before), "same file_id → same chunk ids");
    assert_eq!(
        job.payload,
        Some(serde_json::json!({ "path_refresh": true }))
    );
    assert!(file_id(&fx, id, "old name.md").is_none());
    let rec = fx.cat.read(move |c| files::file(c, before)).unwrap();
    assert_eq!(rec.state, FileState::Indexed, "nothing to re-embed");
    assert!(live_scan(&fx, id).is_none(), "a file rename needs no scan");
    // Same directory (RENAMED_OLD/NEW_NAME pair): same identity, the jobs coalesce.
    std::fs::rename(
        root.join("sub").join("new name.md"),
        root.join("sub").join("final.md"),
    )
    .unwrap();
    wait_for(Duration::from_secs(5), || {
        file_id(&fx, id, "sub/final.md").is_some()
    })
    .await;
    assert_eq!(file_id(&fx, id, "sub/final.md"), Some(before));
    let job = live_job_for(&fx, id, "sub/final.md", JobKind::IndexFile).unwrap();
    assert_eq!(
        job.payload,
        Some(serde_json::json!({ "path_refresh": true }))
    );
    assert_eq!(live_index_jobs_by_file(&fx, id).len(), 1);
    assert!(live_scan(&fx, id).is_none());
    cancel.cancel();
    w.join().await;
}

/// 10k files rewritten twice while the executor is blocked: every file ends with exactly one
/// live index job (none lost, none duplicated), whatever mix of events and storm rescans the
/// watcher produced.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ten_thousand_file_churn_keeps_one_live_job_per_file() {
    const N: usize = 10_000;
    let fx = setup();
    let root = fx.dir("churn");
    for i in 0..N {
        write(&root, &format!("d{:02}/f{i:05}.txt", i % 100), "v0");
    }
    let mut exec = FakeExec::new(&fx.cat);
    let gate = Arc::new(Semaphore::new(0));
    exec.gate = Some(Arc::clone(&gate));
    let svc = fx.service(Arc::new(exec), fx.epoch(), fast_cfg());
    let id = add(svc.reconciler(), &fx, &root);
    svc.reconciler()
        .scan_blocking(id, RunTrigger::Manual, &CancellationToken::new())
        .unwrap();
    index_all_now(&fx, id);
    let (stop, run) = start_loop(&svc);
    let wcancel = CancellationToken::new();
    let w = svc.start_watcher(wcancel.clone());
    wait_for(Duration::from_secs(10), || w.is_watching(id)).await;
    let t0 = Instant::now();
    for round in 1..=2 {
        for i in 0..N {
            write(
                &root,
                &format!("d{:02}/f{i:05}.txt", i % 100),
                &format!("round {round} body {i}"),
            );
        }
    }
    let written = t0.elapsed();
    // Converged: every file has its job, no scan pending, stable for a while.
    let deadline = Instant::now() + Duration::from_secs(240);
    let mut stable_since: Option<Instant> = None;
    loop {
        let by_file = live_index_jobs_by_file(&fx, id);
        let done = by_file.len() == N && live_scan(&fx, id).is_none();
        if done {
            let s = *stable_since.get_or_insert_with(Instant::now);
            if s.elapsed() > Duration::from_millis(1500) {
                break;
            }
        } else {
            stable_since = None;
        }
        assert!(
            Instant::now() < deadline,
            "not converged: {} of {N} files have a job, scan live: {}",
            by_file.len(),
            live_scan(&fx, id).is_some()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let by_file = live_index_jobs_by_file(&fx, id);
    assert_eq!(by_file.len(), N);
    assert!(by_file.values().all(|n| *n == 1), "one live job per file");
    let total_index_jobs = fx
        .live_jobs(Some(id))
        .iter()
        .filter(|j| j.kind == JobKind::IndexFile)
        .count();
    assert_eq!(total_index_jobs, N);
    let c = w.counters();
    eprintln!(
        "churn: {N} files x2 written in {written:?}, converged after {:?}; {c:?}",
        t0.elapsed()
    );
    assert!(c.storms >= 1, "a 20k-write burst is a storm");
    // Re-armed after the storm (plus once per > 1 s stall under load), never once per window.
    assert!(
        c.rearms <= 10 && c.rearms * 4 < c.storms.max(8),
        "re-armed after the storm, not per window: {c:?}"
    );
    wcancel.cancel();
    w.join().await;
    stop.cancel();
    run.await.unwrap().unwrap();
}

/// A root renamed away or deleted makes the folder `unavailable` and deletes nothing; when it
/// comes back the folder reconnects, is watched again and keeps its files.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn root_rename_or_delete_marks_unavailable_and_never_deletes() {
    let fx = setup();
    let (svc, id, root) = baseline(
        &fx,
        "movable",
        &[("a.md", "a"), ("b.md", "b"), ("deep/c.md", "c")],
    );
    let want = vec!["a.md", "b.md", "deep/c.md"];
    let (stop, run) = start_loop(&svc);
    let wc = CancellationToken::new();
    let w = svc.start_watcher(wc.clone());
    wait_for(Duration::from_secs(10), || w.is_watching(id)).await;

    // Rename the root away.
    let moved = root.with_file_name("movable-renamed");
    std::fs::rename(&root, &moved).unwrap();
    wait_for(Duration::from_secs(10), || !fx.folder(id).available).await;
    assert_eq!(fx.file_paths(id), want);
    wait_for(Duration::from_secs(5), || !w.is_watching(id)).await;
    let st = fx.cat.read(move |c| status::counts(c, id)).unwrap();
    assert_eq!(status::derive(&st), FolderStatus::Unavailable);
    // Back: reconnected, watched, nothing lost.
    std::fs::rename(&moved, &root).unwrap();
    wait_for(Duration::from_secs(10), || fx.folder(id).available).await;
    wait_for(Duration::from_secs(10), || w.is_watching(id)).await;
    wait_for(Duration::from_secs(10), || fx.queue_depth() == (0, 0)).await;
    assert_eq!(fx.file_paths(id), want);

    // Delete the root (its files' removals are reported first).
    std::fs::remove_dir_all(&root).unwrap();
    wait_for(Duration::from_secs(10), || !fx.folder(id).available).await;
    tokio::time::sleep(Duration::from_millis(
        u64::try_from(sr_sync::reconcile::DELETE_GRACE_MS).unwrap() + 1500,
    ))
    .await;
    assert_eq!(
        fx.file_paths(id),
        want,
        "a lost root never empties the folder"
    );
    // Restored (e.g. from the Recycle Bin): same files, same content.
    for (rel, body) in [("a.md", "a"), ("b.md", "b"), ("deep/c.md", "c")] {
        write(&root, rel, body);
    }
    wait_for(Duration::from_secs(10), || fx.folder(id).available).await;
    wait_for(Duration::from_secs(15), || {
        fx.queue_depth() == (0, 0) && fx.folder(id).status == FolderStatus::Synced
    })
    .await;
    assert_eq!(fx.file_paths(id), want);
    eprintln!("root loss: {:?}", w.counters());
    assert!(w.counters().roots_lost >= 1 && w.counters().roots_back >= 1);
    wc.cancel();
    w.join().await;
    stop.cancel();
    run.await.unwrap().unwrap();
}

// -------------------------------------------------------------- overflow and storms (sync layer)

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overflow_storm_and_queue_overflow_each_become_one_rescan() {
    let fx = setup();
    let (svc, a, _) = baseline(&fx, "ovf", &[("x.md", "x")]);
    let b = add(svc.reconciler(), &fx, &fx.dir("storm"));
    svc.reconciler()
        .scan_blocking(b, RunTrigger::Manual, &CancellationToken::new())
        .unwrap();
    let cancel = CancellationToken::new();
    let w = svc.start_watcher(cancel.clone());
    wait_for(Duration::from_secs(10), || {
        w.is_watching(a) && w.is_watching(b)
    })
    .await;

    // 1. OS buffer overflow (what notify 9 reports as Flag::Rescan): one watcher scan.
    let t0 = Instant::now();
    w.simulate_overflow(a);
    wait_for(Duration::from_secs(2), || live_scan(&fx, a).is_some()).await;
    let scan = live_scan(&fx, a).unwrap();
    assert_eq!(
        scan.payload,
        Some(serde_json::json!({ "trigger": "watcher" }))
    );
    eprintln!("overflow -> rescan job in {:?}", t0.elapsed());
    assert_eq!(w.counters().overflows, 1);

    // 2. A storm (more raw events in one window than the OS buffer could hold): the batch is
    //    replaced by one rescan, no per-event work.
    let storm = sr_sync::WatchConfig::default().storm_events + 44;
    for i in 0..storm {
        assert!(w.inject(b, PathEvent::Changed(format!("burst{i}.md"))));
    }
    wait_for(Duration::from_secs(2), || live_scan(&fx, b).is_some()).await;
    assert_eq!(w.counters().storms, 1);
    assert!(
        live_index_jobs_by_file(&fx, b).is_empty(),
        "storm events are not processed one by one"
    );
    cancel.cancel();
    w.join().await;

    // 3. Our own queue overflowing (OS thread never blocks): dropped events → rescan.
    let mut cfg = fast_cfg();
    cfg.watch.queue_capacity = 16;
    let fx2 = setup();
    let root = fx2.dir("q");
    let svc2 = fx2.service(Arc::new(FakeExec::new(&fx2.cat)), fx2.epoch(), cfg);
    let c = add(svc2.reconciler(), &fx2, &root);
    svc2.reconciler()
        .scan_blocking(c, RunTrigger::Manual, &CancellationToken::new())
        .unwrap();
    let cancel = CancellationToken::new();
    let w2 = svc2.start_watcher(cancel.clone());
    let rejected = (0..2_000)
        .filter(|i| !w2.inject(c, PathEvent::Changed(format!("q{i}.md"))))
        .count();
    assert!(rejected > 0, "a 16-slot queue must overflow");
    wait_for(Duration::from_secs(2), || live_scan(&fx2, c).is_some()).await;
    assert!(w2.counters().overflows >= 1);
    cancel.cancel();
    w2.join().await;
}

/// Watcher events for paused folders and folders whose sync mode doesn't take them are ignored.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_folders_whose_policy_allows_watcher_events_are_watched() {
    let fx = setup();
    let exec = Arc::new(FakeExec::new(&fx.cat));
    let svc = fx.service(exec, fx.epoch(), fast_cfg());
    let mut ids = Vec::new();
    for (name, mode) in [
        ("auto", SyncMode::Auto),
        ("triggered", SyncMode::Triggered),
        ("scheduled", SyncMode::Scheduled),
        ("manual", SyncMode::Manual),
    ] {
        let mut req = sr_sync::AddFolder::new(fx.collection, fx.dir(name));
        req.settings = FolderSettings {
            sync_mode: mode,
            ..FolderSettings::default()
        };
        ids.push(svc.reconciler().add_folder(&req).unwrap());
    }
    let cancel = CancellationToken::new();
    let w = svc.start_watcher(cancel.clone());
    wait_for(Duration::from_secs(10), || w.watched().len() == 2).await;
    assert!(w.is_watching(ids[0]) && w.is_watching(ids[1]));
    // Pause stops watching; resume restarts it.
    svc.pause(ids[0]).await.unwrap();
    wait_for(Duration::from_secs(5), || !w.is_watching(ids[0])).await;
    assert!(
        svc.resume(ids[0]).await.unwrap(),
        "auto folder gets a catch-up scan"
    );
    wait_for(Duration::from_secs(5), || w.is_watching(ids[0])).await;
    cancel.cancel();
    w.join().await;
}

// ------------------------------------------------------------------- pause / resume / cancel

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_releases_running_jobs_holds_queued_ones_and_resume_finishes_them() {
    let fx = setup();
    let root = fx.dir("p");
    for i in 0..6 {
        write(&root, &format!("f{i}.md"), &format!("body {i}"));
    }
    let mut exec = FakeExec::new(&fx.cat);
    let gate = Arc::new(Semaphore::new(0));
    exec.gate = Some(Arc::clone(&gate));
    let exec = Arc::new(exec);
    let mut cfg = fast_cfg();
    cfg.pipeline_permits = 2;
    let svc = fx.service(Arc::clone(&exec), fx.epoch(), cfg);
    let id = add(svc.reconciler(), &fx, &root);
    svc.reconciler()
        .scan_blocking(id, RunTrigger::Manual, &CancellationToken::new())
        .unwrap();
    let (stop, run) = start_loop(&svc);
    wait_for(Duration::from_secs(5), || {
        exec.running.load(Ordering::SeqCst) == 2
    })
    .await;
    svc.pause(id).await.unwrap();
    wait_for(Duration::from_secs(5), || fx.queue_depth() == (6, 0)).await;
    assert!(
        fx.live_jobs(Some(id)).iter().all(|j| j.attempts == 0),
        "released without using an attempt"
    );
    assert_eq!(fx.folder(id).status, FolderStatus::Paused);
    // Nothing runs while paused, even with capacity.
    gate.add_permits(100);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        exec.order().len(),
        2,
        "only the two that were running before the pause"
    );
    assert_eq!(fx.queue_depth(), (6, 0));
    assert!(fx.folder(id).paused_at.is_some());
    // Resume: everything finishes, each job ran to completion once.
    assert!(svc.resume(id).await.unwrap());
    wait_for(Duration::from_secs(10), || {
        fx.queue_depth() == (0, 0) && fx.folder(id).status == FolderStatus::Synced
    })
    .await;
    let attempts: Vec<i64> = fx
        .cat
        .read(|c| {
            let mut st = c
                .prepare(
                    "SELECT attempts FROM job_history WHERE kind = 'index_file' AND state = 'done'",
                )
                .map_err(sr_catalog::map_db_err)?;
            let v = st
                .query_map([], |r| r.get(0))
                .map_err(sr_catalog::map_db_err)?
                .collect::<rusqlite::Result<Vec<i64>>>()
                .map_err(sr_catalog::map_db_err)?;
            Ok(v)
        })
        .unwrap();
    assert_eq!(attempts.len(), 6);
    assert!(
        attempts.iter().all(|a| *a == 1),
        "pause never consumed an attempt: {attempts:?}"
    );
    stop.cancel();
    run.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_stops_the_run_shows_partial_and_the_next_sync_finishes() {
    let fx = setup();
    let root = fx.dir("c");
    for i in 0..20 {
        write(&root, &format!("f{i:02}.md"), &format!("body {i}"));
    }
    let mut exec = FakeExec::new(&fx.cat);
    let gate = Arc::new(Semaphore::new(0));
    exec.gate = Some(Arc::clone(&gate));
    let exec = Arc::new(exec);
    let mut cfg = fast_cfg();
    cfg.pipeline_permits = 2;
    let svc = fx.service(Arc::clone(&exec), fx.epoch(), cfg);
    let id = add(svc.reconciler(), &fx, &root);
    svc.reconciler()
        .scan_blocking(id, RunTrigger::Manual, &CancellationToken::new())
        .unwrap();
    let (stop, run) = start_loop(&svc);
    wait_for(Duration::from_secs(5), || {
        exec.running.load(Ordering::SeqCst) == 2
    })
    .await;
    let r = svc.cancel(id).await.unwrap();
    assert_eq!((r.dropped, r.stopped), (18, 2));
    wait_for(Duration::from_secs(5), || fx.queue_depth() == (0, 0)).await;
    // The cached status is refreshed right after the last stopped job is dropped (pushed, not
    // polled); the live derivation is already `partial` once the queue is empty.
    let live = fx.cat.read(move |c| status::counts(c, id)).unwrap();
    assert_eq!(status::derive(&live), FolderStatus::Partial);
    wait_for(Duration::from_secs(5), || {
        fx.folder(id).status == FolderStatus::Partial
    })
    .await;
    let f = fx.folder(id);
    assert!(f.cancelled_at.is_some());
    assert_eq!(
        f.status,
        FolderStatus::Partial,
        "stopped, not 'indexing' forever"
    );
    let cancelled: i64 = fx
        .cat
        .read(|c| {
            c.query_row(
                "SELECT count(*) FROM job_history WHERE state = 'cancelled'",
                [],
                |r| r.get(0),
            )
            .map_err(sr_catalog::map_db_err)
        })
        .unwrap();
    assert_eq!(cancelled, 20);
    // The next sync picks up where the run stopped and clears the mark.
    gate.add_permits(1_000);
    assert!(svc.request_scan(id, RunTrigger::Manual).await.unwrap());
    wait_for(Duration::from_secs(10), || {
        fx.queue_depth() == (0, 0) && fx.folder(id).status == FolderStatus::Synced
    })
    .await;
    assert!(fx.folder(id).cancelled_at.is_none());
    let n = fx.cat.read(move |c| status::counts(c, id)).unwrap();
    assert_eq!(n.indexed_files, 20);
    stop.cancel();
    run.await.unwrap().unwrap();
}

// ------------------------------------------------------------------------------- approvals

fn mcp(fx: &Fx, path: &Path) -> McpAddFolder {
    McpAddFolder {
        client_id: "claude-1".into(),
        client_kind: ClientKind::ClaudeDesktop,
        client_name: None,
        path: path.to_path_buf(),
        collection_id: fx.collection,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mcp_add_folder_is_off_by_default_and_needs_approval_when_on() {
    let fx = setup();
    let secret = dunce::canonicalize(fx.dir("private")).unwrap();
    let mut cfg = fast_cfg();
    cfg.dangerous = sr_common::sandbox::DangerousRoots::none().with_never_inside(&secret);
    cfg.approval_ttl = Duration::from_millis(300);
    assert!(
        !sr_sync::SyncConfig::default().mcp_add_folder_enabled,
        "off by default (P1)"
    );
    let svc = fx.service(Arc::new(FakeExec::new(&fx.cat)), fx.epoch(), cfg);
    let wanted = fx.dir("projects");
    write(&wanted, "readme.md", "hi");

    // Off: refused, nothing recorded, nothing pushed.
    let e = svc.request_add_folder(mcp(&fx, &wanted)).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::ApprovalDenied);
    assert!(svc.pending_approvals().await.unwrap().is_empty());

    svc.set_mcp_add_folder_enabled(true).await.unwrap();
    std::fs::create_dir_all(secret.join("sub")).unwrap();
    // Impossible requests fail before the user is bothered.
    let e = svc
        .request_add_folder(mcp(&fx, &secret.join("sub")))
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::PathRefused);
    let e = svc
        .request_add_folder(mcp(&fx, &fx.data.path().join("does-not-exist")))
        .await
        .unwrap_err();
    assert!(
        matches!(e.code, ErrorCode::FolderUnavailable | ErrorCode::NotFound),
        "{e:?}"
    );

    // A valid request is pending, pushed once, and deduplicated.
    let t = svc.request_add_folder(mcp(&fx, &wanted)).await.unwrap();
    assert!(t.created);
    let again = svc.request_add_folder(mcp(&fx, &wanted)).await.unwrap();
    assert!(!again.created && again.id == t.id);
    let pushed = fx
        .events()
        .into_iter()
        .filter(|e| matches!(e.payload, EngineEvent::ApprovalRequested { approval_id, .. } if approval_id == t.id))
        .count();
    assert_eq!(pushed, 1);
    let err = t.to_error();
    assert_eq!(err.code, ErrorCode::ApprovalRequired);
    assert_eq!(err.ctx.approval_id, Some(t.id));
    assert!(
        fx.cat.read(folders::folders).unwrap().is_empty(),
        "nothing added yet"
    );

    // Approve: added like from the app, first scan queued (auto), result recorded.
    let folder = svc.approve(t.id).await.unwrap();
    assert_eq!(
        fx.folder(folder).root_path,
        dunce::canonicalize(&wanted).unwrap()
    );
    assert!(live_scan(&fx, folder).is_some());
    let rec = svc.approval(t.id).await.unwrap();
    assert_eq!(
        (rec.state, rec.folder_id),
        (ApprovalState::Approved, Some(folder))
    );
    assert_eq!(
        svc.approve(t.id).await.unwrap_err().code,
        ErrorCode::Conflict
    );

    // Overlapping request: refused up front ("Already included in ...").
    std::fs::create_dir_all(wanted.join("inner")).unwrap();
    let e = svc
        .request_add_folder(mcp(&fx, &wanted.join("inner")))
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::FolderOverlap);

    // Deny, and expiry.
    let other = fx.dir("other");
    let t2 = svc.request_add_folder(mcp(&fx, &other)).await.unwrap();
    svc.deny(t2.id).await.unwrap();
    assert_eq!(
        svc.approval(t2.id).await.unwrap().state,
        ApprovalState::Denied
    );
    let third = fx.dir("third");
    let t3 = svc.request_add_folder(mcp(&fx, &third)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    let tick = svc.scheduler_tick(sr_catalog::now_ms()).await.unwrap();
    assert_eq!(tick.approvals_expired, 1);
    assert_eq!(
        svc.approve(t3.id).await.unwrap_err().code,
        ErrorCode::ApprovalDenied
    );
    assert!(fx.cat.read(folders::folders).unwrap().len() == 1);

    // Turning the feature off expires what is pending.
    let fourth = fx.dir("fourth");
    let t4 = svc.request_add_folder(mcp(&fx, &fourth)).await.unwrap();
    svc.set_mcp_add_folder_enabled(false).await.unwrap();
    assert_eq!(
        svc.approval(t4.id).await.unwrap().state,
        ApprovalState::Expired
    );
    let _ = approvals::DEFAULT_TTL_MS;
}

// ----------------------------------------------------------- network drives, reconcile-only

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn network_folders_are_reconcile_only_with_a_plain_note() {
    let fx = setup();
    let exec = Arc::new(FakeExec::new(&fx.cat));
    let svc = fx.service(exec, fx.epoch(), fast_cfg());
    let mut req = sr_sync::AddFolder::new(fx.collection, fx.dir("share"));
    req.settings.sync_mode = SyncMode::Triggered; // watcher-only mode
    let id = svc.reconciler().add_folder(&req).unwrap();
    assert_eq!(
        fx.folder(id).watch_support,
        WatchSupport::Native,
        "a local temp dir"
    );
    assert_eq!(sr_sync::watch_note(&fx.folder(id)), None);
    // As if the root were on a mapped network drive (GetDriveTypeW = DRIVE_REMOTE).
    fx.cat
        .write_blocking(move |c| {
            folders::set_watch_support(c, id, WatchSupport::Network, sr_catalog::now_ms())
        })
        .unwrap();
    let f = fx.folder(id);
    let b = sr_sync::reconcile::behavior_of(&f);
    assert!(!b.watch, "no watcher on a network drive");
    assert_eq!(
        b.reconcile_every_min,
        Some(60),
        "watcher-only mode reconciles instead"
    );
    assert_eq!(
        sr_sync::watch_note(&f).as_deref(),
        Some("This folder is on a network drive, so changes are picked up every hour.")
    );
    let due = svc.schedule_due(sr_catalog::now_ms()).await.unwrap();
    assert_eq!(
        due,
        vec![id],
        "scheduled although the sync mode is watcher-only"
    );
    let cancel = CancellationToken::new();
    let w = svc.start_watcher(cancel.clone());
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(!w.is_watching(id));
    // Re-validation sees a local drive again (drive remapped): watched.
    svc.reconciler().check_root(&f).unwrap();
    assert_eq!(fx.folder(id).watch_support, WatchSupport::Native);
    w.refresh();
    wait_for(Duration::from_secs(5), || w.is_watching(id)).await;
    // "unsupported" (watcher failed on that file system) reads differently.
    fx.cat
        .write_blocking(move |c| {
            folders::set_watch_support(c, id, WatchSupport::Unsupported, sr_catalog::now_ms())
        })
        .unwrap();
    let mut f = fx.folder(id);
    f.settings.cadence_min = 15;
    assert_eq!(
        sr_sync::watch_note(&f).as_deref(),
        Some(
            "Live updates aren't available for this drive, so changes are picked up every 15 minutes."
        )
    );
    cancel.cancel();
    w.join().await;
}

// ------------------------------------------------------------------- 64-bit volume serials

#[test]
fn legacy_32_bit_volume_serial_is_upgraded_not_treated_as_another_drive() {
    let fx = setup();
    let root = fx.dir("vs");
    write(&root, "a.md", "a");
    let exec = Arc::new(FakeExec::new(&fx.cat));
    let svc = fx.service(exec, 1, fast_cfg());
    let id = add(svc.reconciler(), &fx, &root);
    svc.reconciler()
        .scan_blocking(id, RunTrigger::Manual, &CancellationToken::new())
        .unwrap();
    let full = fx.folder(id).volume_serial.unwrap();
    let file = file_id(&fx, id, "a.md").unwrap();
    if full == full & 0xFFFF_FFFF {
        eprintln!(
            "volume serial fits 32 bits on this machine ({full:#x}); upgrade path not exercised"
        );
        return;
    }
    // Simulate a catalog written before V3: low 32 bits only, folder and file.
    let low = sr_catalog::model::vs_to_sql(full & 0xFFFF_FFFF);
    fx.cat
        .write_blocking(move |c| {
            c.execute("UPDATE folders SET volume_serial = ?1", [low])
                .map_err(sr_catalog::map_db_err)?;
            c.execute("UPDATE files SET volume_serial = ?1", [low])
                .map_err(sr_catalog::map_db_err)?;
            Ok(())
        })
        .unwrap();
    let f = fx.folder(id);
    assert!(matches!(
        svc.reconciler().check_root(&f).unwrap(),
        sr_sync::reconcile::RootCheck::Ok { .. }
    ));
    let f = fx.folder(id);
    assert!(f.available);
    assert_eq!(f.volume_serial, Some(full), "upgraded in place");
    let hit = fx
        .cat
        .read(move |c| {
            let fid = sr_sync::fsid::identity(&root.join("a.md")).unwrap().file_id;
            files::file_by_identity(c, id, full, fid)
        })
        .unwrap();
    assert_eq!(
        hit.map(|r| r.id),
        Some(file),
        "file rows upgraded too (rename detection)"
    );
    // A really different serial is still another drive.
    let other = sr_catalog::model::vs_to_sql(full ^ 0x1);
    fx.cat
        .write_blocking(move |c| {
            c.execute("UPDATE folders SET volume_serial = ?1", [other])
                .map_err(sr_catalog::map_db_err)?;
            Ok(())
        })
        .unwrap();
    let f = fx.folder(id);
    assert!(matches!(
        svc.reconciler().check_root(&f).unwrap(),
        sr_sync::reconcile::RootCheck::Unavailable(ErrorCode::FolderUnavailable)
    ));
}

// ------------------------------------------------------------------------------- scheduler

/// Folders the watcher doesn't serve (scheduled mode here) reconnect through the scheduler tick.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scheduler_tick_reconnects_a_returning_root_and_schedules_stale_folders() {
    let fx = setup();
    let exec = Arc::new(FakeExec::new(&fx.cat));
    let svc = fx.service(exec, fx.epoch(), fast_cfg());
    let root = fx.dir("usb");
    write(&root, "a.md", "a");
    let mut req = sr_sync::AddFolder::new(fx.collection, &root);
    req.settings.sync_mode = SyncMode::Scheduled;
    let id = svc.reconciler().add_folder(&req).unwrap();
    svc.reconciler()
        .scan_blocking(id, RunTrigger::Manual, &CancellationToken::new())
        .unwrap();
    let now = sr_catalog::now_ms();
    let tick = svc.scheduler_tick(now).await.unwrap();
    assert!(
        tick.due.is_empty() && tick.reconnected.is_empty(),
        "{tick:?}"
    );
    // Unplugged: the next scan marks it unavailable, files kept.
    let away = root.with_file_name("usb-away");
    std::fs::rename(&root, &away).unwrap();
    let out = svc
        .reconciler()
        .scan_blocking(id, RunTrigger::Schedule, &CancellationToken::new())
        .unwrap();
    assert!(matches!(out, sr_sync::ScanOutcome::Unavailable(_)));
    assert_eq!(fx.file_paths(id), vec!["a.md"]);
    let tick = svc.scheduler_tick(now + 2 * 3_600_000).await.unwrap();
    assert!(tick.due.is_empty(), "unavailable folders are not scheduled");
    assert!(tick.reconnected.is_empty());
    // Plugged back in: reconnected with a catch-up scan.
    std::fs::rename(&away, &root).unwrap();
    let tick = svc.scheduler_tick(now + 2 * 3_600_000).await.unwrap();
    assert_eq!(tick.reconnected, vec![id]);
    assert!(fx.folder(id).available);
    let scan = live_scan(&fx, id).expect("catch-up scan");
    assert!(
        scan.payload == Some(serde_json::json!({ "trigger": "reconnect" }))
            || scan.payload == Some(serde_json::json!({ "trigger": "schedule" })),
        "{:?}",
        scan.payload
    );
}
