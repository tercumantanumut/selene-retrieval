//! Claim loop, policy and recovery against a real catalog with a fake executor.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use common::*;
use sr_catalog::repo::{chunks, files, folders, jobs};
use sr_catalog::{Catalog, FileState, JobKind, RunTrigger, SKIP_REASON_FILES_ONLY};
use sr_common::error::{Error, ErrorCode};
use sr_common::events::{EngineEvent, FolderStatus};
use sr_sync::{EventOutcome, JobOutcome, PathEvent, ScanOutcome};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

fn scan_now(svc: &sr_sync::Reconciler, f: sr_common::ids::FolderId) {
    let out = svc
        .scan_blocking(f, RunTrigger::Manual, &CancellationToken::new())
        .unwrap();
    assert!(matches!(out, ScanOutcome::Completed(_)), "{out:?}");
}

#[test]
fn burst_of_500_change_events_leaves_one_index_job() {
    let fx = setup();
    let root = fx.dir("burst");
    let p = write(&root, "notes.md", "v1");
    let svc = fx.service(Arc::new(FakeExec::new(&fx.cat)), fx.epoch(), fast_cfg());
    let rec = svc.reconciler();
    let id = add(rec, &fx, &root);
    scan_now(rec, id);
    std::fs::write(&p, "v2 is longer").unwrap();
    let mut coalesced = 0;
    for i in 0..500 {
        let rel = if i % 2 == 0 { "notes.md" } else { "NOTES.MD" };
        match rec
            .on_path_event(id, &PathEvent::Changed(rel.into()))
            .unwrap()
        {
            EventOutcome::Enqueued {
                kind: JobKind::IndexFile,
                coalesced: c,
            } => coalesced += usize::from(c),
            other => panic!("{other:?}"),
        }
    }
    let live = fx.live_jobs(Some(id));
    let index: Vec<_> = live
        .iter()
        .filter(|j| j.kind == JobKind::IndexFile)
        .collect();
    assert_eq!(index.len(), 1, "{live:?}");
    assert_eq!(coalesced, 500, "the scan's job absorbed every event");
    assert_eq!(fx.file_paths(id), vec!["notes.md"]);

    // Watcher events are ignored when the sync mode does not take them.
    let mut s = fx.folder(id).settings;
    s.sync_mode = sr_common::config::SyncMode::Scheduled;
    rec.update_settings(id, &s).unwrap();
    assert_eq!(
        rec.on_path_event(id, &PathEvent::Changed("notes.md".into()))
            .unwrap(),
        EventOutcome::Ignored
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn drr_keeps_a_small_folder_from_starving_behind_a_big_one() {
    let fx = setup();
    let big = fx.dir("big");
    let small = fx.dir("small");
    for i in 0..120 {
        write(&big, &format!("b{i:03}.md"), "b");
    }
    for i in 0..5 {
        write(&small, &format!("s{i}.md"), "s");
    }
    let exec = Arc::new(FakeExec::new(&fx.cat).with_behave(|_, _| Ok(JobOutcome::Done)));
    let mut cfg = fast_cfg();
    cfg.pipeline_permits = 1;
    let svc = fx.service(exec.clone(), fx.epoch(), cfg);
    let rec = svc.reconciler();
    let b = add(rec, &fx, &big);
    let s = add(rec, &fx, &small);
    scan_now(rec, b); // the big folder's 120 jobs are queued first
    scan_now(rec, s);
    run_until_idle(&fx, &svc, Duration::from_secs(60)).await;
    let order: Vec<_> = exec
        .order()
        .into_iter()
        .filter(|(k, _, _)| *k == JobKind::IndexFile)
        .collect();
    assert_eq!(order.len(), 125);
    let last_small = order.iter().rposition(|(_, f, _)| *f == Some(s)).unwrap();
    assert!(
        last_small < 12,
        "small folder finished at position {last_small}: DRR alternates folders"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backpressure_never_leases_more_jobs_than_permits() {
    let fx = setup();
    let root = fx.dir("bp");
    for i in 0..30 {
        write(&root, &format!("f{i}.md"), "x");
    }
    let mut e = FakeExec::new(&fx.cat).with_behave(|_, _| Ok(JobOutcome::Done));
    e.delay = Duration::from_millis(15);
    let exec = Arc::new(e);
    let mut cfg = fast_cfg();
    cfg.pipeline_permits = 3;
    let svc = fx.service(exec.clone(), fx.epoch(), cfg);
    let id = add(svc.reconciler(), &fx, &root);
    scan_now(svc.reconciler(), id);
    assert_eq!(fx.queue_depth(), (30, 0));
    run_until_idle(&fx, &svc, Duration::from_secs(60)).await;
    assert_eq!(exec.order().len(), 30);
    assert_eq!(exec.max_running.load(Ordering::SeqCst), 3, "permits used");
    assert!(
        exec.max_leased.load(Ordering::SeqCst) <= 3,
        "claims <= permits"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_mid_batch_then_new_epoch_requeues_leased_jobs() {
    let fx = setup();
    let root = fx.dir("crash");
    for i in 0..8 {
        write(&root, &format!("f{i}.md"), &format!("content {i}"));
    }
    // Run 1: the executor hangs; the daemon "crashes" while 3 jobs are leased.
    let e1 = fx.epoch();
    let mut hang = FakeExec::new(&fx.cat);
    hang.gate = Some(Arc::new(Semaphore::new(0)));
    let hang = Arc::new(hang);
    let mut cfg = fast_cfg();
    cfg.pipeline_permits = 3;
    let svc1 = fx.service(hang.clone(), e1, cfg.clone());
    let id = add(svc1.reconciler(), &fx, &root);
    scan_now(svc1.reconciler(), id);
    let s = svc1.clone();
    let h = tokio::spawn(async move { s.run(CancellationToken::new()).await });
    wait_for(Duration::from_secs(10), || {
        hang.running.load(Ordering::SeqCst) == 3
    })
    .await;
    h.abort(); // crash: no drain, no completion
    let _ = h.await;
    drop(svc1);
    assert_eq!(fx.queue_depth(), (5, 3));
    let stuck: Vec<i64> = fx
        .live_jobs(Some(id))
        .into_iter()
        .filter(|j| j.state == sr_catalog::JobState::Leased)
        .map(|j| j.id)
        .collect();

    // Restart: a new epoch fences the old leases.
    let start = fx.cat.start_epoch().unwrap();
    assert_eq!(start.requeued, 3);
    let e2 = start.epoch;
    assert!(e2 > e1);
    let stale = fx
        .cat
        .write_blocking(move |c| jobs::complete(c, stuck[0], e1, sr_catalog::now_ms()));
    assert_eq!(
        stale.unwrap_err().code,
        ErrorCode::Conflict,
        "old holder fenced"
    );

    let good = Arc::new(FakeExec::new(&fx.cat));
    let svc2 = fx.service(good.clone(), e2, cfg);
    svc2.startup().await.unwrap();
    run_until_idle(&fx, &svc2, Duration::from_secs(30)).await;
    let done: Vec<String> = good
        .order()
        .into_iter()
        .filter(|(k, _, _)| *k == JobKind::IndexFile)
        .map(|(_, _, r)| r)
        .collect();
    let unique: HashSet<&String> = done.iter().collect();
    assert_eq!(done.len(), 8, "every file exactly once: {done:?}");
    assert_eq!(unique.len(), 8);
    assert_eq!(fx.folder(id).status, FolderStatus::Synced);
    let indexed = fx
        .cat
        .read(|c| files::files_in_state(c, id, FileState::Indexed))
        .unwrap();
    assert_eq!(indexed.len(), 8);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interrupted_sync_is_never_reported_synced_on_recovery() {
    let fx = setup();
    let root = fx.dir("w2");
    for i in 0..4 {
        write(&root, &format!("f{i}.md"), "x");
    }
    let e1 = fx.epoch();
    let mut hang = FakeExec::new(&fx.cat);
    hang.gate = Some(Arc::new(Semaphore::new(0)));
    let hang = Arc::new(hang);
    let svc1 = fx.service(hang.clone(), e1, fast_cfg());
    let id = svc1
        .add_folder(sr_sync::AddFolder::new(fx.collection, &root))
        .await
        .unwrap();
    let s = svc1.clone();
    let h = tokio::spawn(async move { s.run(CancellationToken::new()).await });
    wait_for(Duration::from_secs(10), || {
        hang.running.load(Ordering::SeqCst) >= 1
    })
    .await;
    h.abort();
    let _ = h.await;
    drop(svc1);

    let e2 = fx.epoch();
    let svc2 = fx.service(Arc::new(FakeExec::new(&fx.cat)), e2, fast_cfg());
    let seq_before = fx.events.last_seq();
    svc2.startup().await.unwrap();
    let f = fx.folder(id);
    assert_eq!(
        f.status,
        FolderStatus::Indexing,
        "recovered work is still indexing"
    );
    assert_eq!(f.last_success_at, None, "never marked synced (Selene W2)");
    let synced_during_recovery = fx.events().into_iter().any(|e| {
        e.seq > seq_before
            && matches!(
                e.payload,
                EngineEvent::FolderStatusChanged {
                    status: FolderStatus::Synced,
                    ..
                }
            )
    });
    assert!(!synced_during_recovery);

    run_until_idle(&fx, &svc2, Duration::from_secs(30)).await;
    let f = fx.folder(id);
    assert_eq!(f.status, FolderStatus::Synced);
    assert!(f.last_success_at.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dead_letter_and_retries_end_in_synced_with_errors() {
    let fx = setup();
    let root = fx.dir("dead");
    write(&root, "good.md", "fine");
    write(&root, "bad.md", "corrupt");
    write(&root, "locked.md", "busy");
    write(&root, "panics.md", "boom");
    let exec = Arc::new(FakeExec::new(&fx.cat).with_behave({
        let cat = Arc::clone(&fx.cat);
        move |job, rel| {
            let name = rel.to_string_lossy().into_owned();
            match name.as_str() {
                "bad.md" => Err(Error::new(ErrorCode::FileUnreadable)),
                "locked.md" => Err(Error::new(ErrorCode::FileLocked)),
                "panics.md" => panic!("parser bug"),
                _ => {
                    // Index for real so the good file ends `indexed`.
                    let file = job.file_id.unwrap();
                    let bytes = b"fine".to_vec();
                    cat.write_blocking(move |c| {
                        let now = sr_catalog::now_ms();
                        let fc = chunks::FileChunks {
                            file_id: file,
                            content_hash: sr_common::ids::ContentHash::of_bytes(&bytes),
                            chunk_cfg_fp: sr_common::ids::ChunkCfgFp::compute("t", "{}", "k", "1"),
                            extractor_version: "1".into(),
                            chunks: vec![chunks::NewChunk {
                                ord: 0,
                                byte_start: 0,
                                byte_end: 4,
                                line_start: 1,
                                line_end: 1,
                                tokens: 1,
                                text: "fine".into(),
                            }],
                        };
                        let t1 = chunks::write_file_chunks(c, &fc, now)?;
                        chunks::finalize_file(c, file, t1.file_version, now)?;
                        Ok(())
                    })?;
                    Ok(JobOutcome::Done)
                }
            }
        }
    }));
    let svc = fx.service(exec.clone(), fx.epoch(), fast_cfg());
    let id = svc
        .add_folder(sr_sync::AddFolder::new(fx.collection, &root))
        .await
        .unwrap();
    run_until_idle(&fx, &svc, Duration::from_secs(30)).await;

    let f = fx.folder(id);
    assert_eq!(f.status, FolderStatus::SyncedWithErrors);
    assert!(f.last_success_at.is_some());
    let mut dead: Vec<String> = fx
        .cat
        .read(|c| files::dead_letter(c, id))
        .unwrap()
        .into_iter()
        .map(|r| r.rel_path)
        .collect();
    dead.sort();
    assert_eq!(dead, vec!["bad.md", "locked.md", "panics.md"]);
    let tries = |name: &str| {
        exec.order()
            .iter()
            .filter(|(k, _, r)| *k == JobKind::IndexFile && r == name)
            .count()
    };
    assert_eq!(tries("bad.md"), 1, "non-retryable: dead at once");
    assert_eq!(
        tries("locked.md"),
        3,
        "retryable: max_attempts with backoff"
    );
    assert_eq!(
        tries("panics.md"),
        3,
        "a panicking executor is retried then dead-lettered"
    );

    let evs = fx.events();
    let failed = evs
        .iter()
        .filter(|e| matches!(e.payload, EngineEvent::FileFailed { .. }))
        .count();
    assert_eq!(failed, 3);
    assert!(evs.iter().any(|e| matches!(
        e.payload,
        EngineEvent::FolderStatusChanged {
            status: FolderStatus::SyncedWithErrors,
            ..
        }
    )));
    assert!(
        evs.iter()
            .any(|e| matches!(e.payload, EngineEvent::IndexProgress { .. }))
    );
    assert!(
        evs.iter()
            .any(|e| matches!(e.payload, EngineEvent::ScanProgress { .. }))
    );
    assert!(
        evs.windows(2).all(|w| w[1].seq == w[0].seq + 1),
        "push events carry seq"
    );
}

/// Amendment 17: a folder an older version left in the removed Files-only mode is paused when the
/// catalog opens (migration V5). Nothing in it is read (no scan runs, no job reaches the executor,
/// no chunk is written) until the user chooses. "Search its contents" (resume) reads every file
/// whatever the folder's sync mode; "Remove" removes it without reading anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_files_only_folder_stays_unread_until_the_user_chooses() {
    let fx = setup();
    let keep = fx.dir("keep");
    let gone = fx.dir("gone");
    for n in ["a.md", "b.md", "c.md"] {
        write(&keep, n, n);
        write(&gone, n, n);
    }
    // What an older version left: both folders listed, nothing read, the catalog at V4. `keep`
    // is checked only when the user clicks (manual).
    let (k, g) = {
        let svc = fx.service(Arc::new(FakeExec::new(&fx.cat)), fx.epoch(), fast_cfg());
        let rec = svc.reconciler();
        let mut req = sr_sync::AddFolder::new(fx.collection, &keep);
        req.settings.keep_up_to_date = false;
        let k = rec.add_folder(&req).unwrap();
        let g = add(rec, &fx, &gone);
        scan_now(rec, k);
        scan_now(rec, g);
        fx.cat
            .write_blocking(move |c| {
                sr_catalog::atomic(c, |c| {
                    let now = sr_catalog::now_ms();
                    for f in [k, g] {
                        for r in files::files_in_state(c, f, FileState::PendingIndex)? {
                            files::mark_skipped(c, r.id, SKIP_REASON_FILES_ONLY, None, now)?;
                        }
                    }
                    c.execute_batch(
                        "DELETE FROM jobs; UPDATE folders SET indexing_mode = 'files_only';",
                    )
                    .map_err(sr_catalog::map_db_err)
                })
            })
            .unwrap();
        fx.cat
            .write_blocking(|c| {
                c.execute_batch("PRAGMA user_version = 4;")
                    .map_err(sr_catalog::map_db_err)
            })
            .unwrap();
        (k, g)
    };
    let Fx {
        cat,
        collection,
        events,
        db_path,
        data,
        db_dir,
    } = fx;
    drop(cat);
    let fx = Fx {
        cat: Arc::new(Catalog::open(&db_path).unwrap()),
        collection,
        events,
        db_path,
        data,
        db_dir,
    };
    assert_eq!(
        fx.cat.schema_version().unwrap(),
        i64::try_from(sr_catalog::schema::LATEST_VERSION).unwrap()
    );
    for f in [k, g] {
        let r = fx.folder(f);
        assert!(r.paused && r.was_files_only, "{r:?}");
    }

    // Start-up, "Check for changes now", an MCP sync request and a settings change read nothing.
    let exec = Arc::new(FakeExec::new(&fx.cat));
    let svc = fx.service(Arc::clone(&exec), fx.epoch(), fast_cfg());
    assert_eq!(svc.startup().await.unwrap().scans_enqueued, 0);
    assert!(!svc.request_scan(k, RunTrigger::Manual).await.unwrap());
    assert!(!svc.request_scan(g, RunTrigger::Mcp).await.unwrap());
    let mut s = fx.folder(g).settings;
    s.file_types_preset = sr_common::config::FileTypesPreset::AllText;
    assert!(svc.update_settings(g, s).await.unwrap(), "its scan is held");
    let cancel = CancellationToken::new();
    let run = {
        let (s, c) = (svc.clone(), cancel.clone());
        tokio::spawn(async move { s.run(c).await })
    };
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(exec.order().is_empty(), "read: {:?}", exec.order());
    for f in [k, g] {
        assert_eq!(fx.folder(f).status, FolderStatus::Paused);
        let listed = fx.cat.read(|c| files::listed_files(c, f)).unwrap();
        assert_eq!(listed.len(), 3, "still listed");
        for r in listed {
            assert!(
                fx.cat
                    .read(|c| chunks::file_chunks(c, r.id))
                    .unwrap()
                    .is_empty()
            );
        }
    }

    // "Search its contents": every file is read, though the folder is checked only on a click.
    assert!(svc.resume(k).await.unwrap(), "a scan is queued");
    assert!(!fx.folder(k).was_files_only, "asked once");
    // (The status alone can read "synced" for a moment before the queued scan starts.)
    let indexed = || {
        fx.cat
            .read(|c| files::files_in_state(c, k, FileState::Indexed))
            .unwrap()
            .len()
    };
    wait_for(Duration::from_secs(10), || {
        indexed() == 3
            && fx.live_jobs(Some(k)).is_empty()
            && fx.folder(k).status == FolderStatus::Synced
    })
    .await;
    assert!(
        fx.cat
            .read(|c| files::listed_files(c, k))
            .unwrap()
            .is_empty()
    );

    // "Remove": the folder goes, nothing in it was ever read.
    svc.remove_folder(g).await.unwrap();
    let gone_err = fx.cat.read(|c| folders::folder(c, g)).unwrap_err();
    assert_eq!(gone_err.code, ErrorCode::NotFound);
    tokio::time::sleep(Duration::from_millis(200)).await;
    cancel.cancel();
    run.await.unwrap().unwrap();
    let order = exec.order();
    assert_eq!(order.len(), 3, "{order:?}");
    assert!(
        order
            .iter()
            .all(|(kind, f, _)| *kind == JobKind::IndexFile && *f == Some(k)),
        "{order:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scheduler_and_add_policy_follow_sync_mode() {
    let fx = setup();
    let auto = fx.dir("auto");
    let manual = fx.dir("manual");
    let triggered = fx.dir("triggered");
    write(&auto, "a.md", "a");
    write(&manual, "m.md", "m");
    let svc = fx.service(Arc::new(FakeExec::new(&fx.cat)), fx.epoch(), fast_cfg());
    let mut req = sr_sync::AddFolder::new(fx.collection, &auto);
    req.settings.cadence_min = 5;
    let a = svc.add_folder(req).await.unwrap();
    let mut req = sr_sync::AddFolder::new(fx.collection, &manual);
    req.settings.keep_up_to_date = false;
    let m = svc.add_folder(req).await.unwrap();
    let mut req = sr_sync::AddFolder::new(fx.collection, &triggered);
    req.settings.sync_mode = sr_common::config::SyncMode::Triggered;
    let t = svc.add_folder(req).await.unwrap();
    // allowsAutomaticAddSync: only `auto` scans on add.
    let scans = |f| {
        fx.live_jobs(Some(f))
            .iter()
            .filter(|j| j.kind == JobKind::ScanFolder)
            .count()
    };
    assert_eq!((scans(a), scans(m), scans(t)), (1, 0, 0));
    assert!(!svc.request_scan(m, RunTrigger::Schedule).await.unwrap());
    assert!(svc.request_scan(m, RunTrigger::Manual).await.unwrap());
    assert!(!svc.request_scan(t, RunTrigger::Schedule).await.unwrap());
    run_until_idle(&fx, &svc, Duration::from_secs(30)).await;

    let now = sr_catalog::now_ms();
    assert!(
        svc.schedule_due(now).await.unwrap().is_empty(),
        "just scanned"
    );
    let due = svc.schedule_due(now + 5 * 60_000).await.unwrap();
    assert_eq!(
        due,
        vec![a],
        "cadence 5 min honoured; manual/triggered never scheduled"
    );
    run_until_idle(&fx, &svc, Duration::from_secs(30)).await;
    assert_eq!(fx.folder(a).last_run_trigger, Some(RunTrigger::Schedule));
}
