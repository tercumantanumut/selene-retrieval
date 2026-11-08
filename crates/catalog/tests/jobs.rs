//! Job queue: dedupe under an event storm, epoch fencing, claim fairness, backoff, dead-letter.
// Test crate: helpers outside #[test] fns may unwrap (clippy.toml covers only #[test] fns).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use sr_catalog::repo::jobs::{
    self, ClaimRequest, Completed, Drr, FailOutcome, JobFailure, NewJob, RetryPolicy,
};
use sr_catalog::repo::{files, meta, status};
use sr_catalog::{FileState, JobKind, JobState};
use sr_common::error::ErrorCode;
use sr_common::events::FolderStatus;

fn failure(code: ErrorCode, retryable: bool) -> JobFailure {
    JobFailure {
        code,
        message: format!("{code}"),
        retryable,
        retry_after_ms: None,
    }
}

#[test]
fn storm_of_500_events_for_one_file_leaves_one_live_job() {
    let fx = setup();
    let folder = add_folder(&fx.cat, fx.collection, &fx.root("storm"));
    let file = add_file(&fx.cat, folder, "hot.log", 1);
    let outcomes = fx
        .cat
        .write_blocking(move |c| {
            (0..500)
                .map(|i| {
                    // Watcher events arrive at priority 1; one interactive request in the middle.
                    let p = if i == 250 { 0 } else { 1 };
                    let mut j = NewJob::new(JobKind::IndexFile, Some(folder), Some(file), p);
                    j.run_at = Some(NOW + 500 - i);
                    jobs::enqueue(c, &j, NOW + i)
                })
                .collect::<sr_common::Result<Vec<_>>>()
        })
        .unwrap();
    assert!(!outcomes[0].coalesced);
    assert!(
        outcomes[1..]
            .iter()
            .all(|o| o.coalesced && o.id == outcomes[0].id)
    );
    let live = fx
        .cat
        .read(move |c| jobs::live_jobs(c, Some(folder)))
        .unwrap();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].priority, 0, "most urgent priority wins");
    assert_eq!(live[0].next_run_at, NOW + 1, "earliest run time wins");
}

#[test]
fn event_while_running_sets_rerun_instead_of_a_second_job() {
    let fx = setup();
    let folder = add_folder(&fx.cat, fx.collection, &fx.root("rerun"));
    let file = add_file(&fx.cat, folder, "a.md", 1);
    let epoch = fx.cat.start_epoch().unwrap().epoch;
    let leased = fx
        .cat
        .write_blocking(move |c| {
            jobs::enqueue(
                c,
                &NewJob::new(JobKind::IndexFile, Some(folder), Some(file), 1),
                NOW,
            )?;
            jobs::claim(c, &ClaimRequest::new(epoch, 4, NOW), &mut Drr::default())
        })
        .unwrap();
    assert_eq!(leased.len(), 1);
    let id = leased[0].id;
    let (again, done) = fx
        .cat
        .write_blocking(move |c| {
            let again = jobs::enqueue(
                c,
                &NewJob::new(JobKind::IndexFile, Some(folder), Some(file), 1),
                NOW + 1,
            )?;
            let done = jobs::complete(c, id, epoch, NOW + 2)?;
            Ok((again, done))
        })
        .unwrap();
    assert!(again.coalesced && again.id == id);
    assert_eq!(
        done,
        Completed::Requeued,
        "the change seen during the run is not lost"
    );
    let done = fx
        .cat
        .write_blocking(move |c| {
            let j = jobs::claim(
                c,
                &ClaimRequest::new(epoch, 4, NOW + 3),
                &mut Drr::default(),
            )?;
            assert_eq!(j.len(), 1);
            jobs::complete(c, id, epoch, NOW + 4)
        })
        .unwrap();
    assert_eq!(done, Completed::Done);
    assert!(
        fx.cat
            .read(move |c| jobs::live_jobs(c, None))
            .unwrap()
            .is_empty()
    );
    let hist: i64 = fx
        .cat
        .read(|c| {
            c.query_row("SELECT count(*) FROM job_history", [], |r| r.get(0))
                .map_err(sr_catalog::map_db_err)
        })
        .unwrap();
    assert_eq!(hist, 1);
}

#[test]
fn epoch_fencing_requeues_old_leases_and_rejects_stale_holders() {
    let mut fx = setup();
    let folder = add_folder(&fx.cat, fx.collection, &fx.root("epoch"));
    let e1 = fx.cat.start_epoch().unwrap();
    assert_eq!(
        e1,
        meta::EpochStart {
            epoch: 1,
            requeued: 0
        }
    );
    let claimed = fx
        .cat
        .write_blocking(move |c| {
            for i in 0..3u8 {
                let f = files::observe(
                    c,
                    folder,
                    files::begin_scan(c, folder, NOW)?,
                    &obs(&format!("f{i}.md"), i),
                    NOW,
                )?;
                jobs::enqueue(
                    c,
                    &NewJob::new(JobKind::IndexFile, Some(folder), Some(f.file_id), 2),
                    NOW,
                )?;
            }
            jobs::claim(c, &ClaimRequest::new(1, 2, NOW), &mut Drr::default())
        })
        .unwrap();
    assert_eq!(claimed.len(), 2);
    assert!(
        claimed
            .iter()
            .all(|j| j.lease_owner == Some(1) && j.state == JobState::Leased)
    );
    // Crash: the daemon dies with two leases; next start fences them.
    fx.reopen();
    let e2 = fx.cat.start_epoch().unwrap();
    assert_eq!(
        e2,
        meta::EpochStart {
            epoch: 2,
            requeued: 2
        }
    );
    let live = fx
        .cat
        .read(move |c| jobs::live_jobs(c, Some(folder)))
        .unwrap();
    assert!(
        live.iter()
            .all(|j| j.state == JobState::Queued && j.lease_owner.is_none())
    );
    // A zombie from epoch 1 can neither complete nor fail its old lease.
    let stale = claimed[0].id;
    let e = fx
        .cat
        .write_blocking(move |c| jobs::complete(c, stale, 1, NOW))
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::Conflict);
    let e = fx
        .cat
        .write_blocking(move |c| {
            jobs::fail(
                c,
                stale,
                1,
                &failure(ErrorCode::FileLocked, true),
                &RetryPolicy::default(),
                NOW,
            )
        })
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::Conflict);
    // Epoch 2 re-claims all three; attempts count the crashed claim too.
    let again = fx
        .cat
        .write_blocking(move |c| {
            jobs::claim(c, &ClaimRequest::new(2, 10, NOW), &mut Drr::default())
        })
        .unwrap();
    assert_eq!(again.len(), 3);
    assert!(again.iter().any(|j| j.id == stale && j.attempts == 2));
    // Even a stale epoch-1 lease that the fencing missed is rejected by owner check.
    let id = again[0].id;
    let e = fx
        .cat
        .write_blocking(move |c| jobs::complete(c, id, 1, NOW))
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::Conflict);
}

#[test]
fn retryable_failure_backs_off_then_dead_letters_the_file() {
    let fx = setup();
    let folder = add_folder(&fx.cat, fx.collection, &fx.root("dead"));
    let file = add_file(&fx.cat, folder, "locked.docx", 1);
    let epoch = fx.cat.start_epoch().unwrap().epoch;
    let policy = RetryPolicy {
        max_attempts: 3,
        base_ms: 1000,
        max_ms: 60_000,
    };
    fx.cat
        .write_blocking(move |c| {
            jobs::enqueue(
                c,
                &NewJob::new(JobKind::IndexFile, Some(folder), Some(file), 1),
                NOW,
            )
        })
        .unwrap();
    let mut now = NOW;
    let mut outcomes = Vec::new();
    for _ in 0..3 {
        let o = fx
            .cat
            .write_blocking(move |c| {
                // Not runnable before its backoff elapses.
                assert!(
                    jobs::claim(
                        c,
                        &ClaimRequest::new(epoch, 1, now - 1),
                        &mut Drr::default()
                    )?
                    .is_empty()
                        || now == NOW
                );
                let j = jobs::claim(c, &ClaimRequest::new(epoch, 1, now), &mut Drr::default())?;
                assert_eq!(j.len(), 1);
                jobs::fail(
                    c,
                    j[0].id,
                    epoch,
                    &failure(ErrorCode::FileLocked, true),
                    &policy,
                    now,
                )
            })
            .unwrap();
        if let FailOutcome::Retrying { next_run_at } = o {
            now = next_run_at;
        }
        outcomes.push(o);
    }
    assert_eq!(
        outcomes,
        vec![
            FailOutcome::Retrying {
                next_run_at: NOW + 1000
            },
            FailOutcome::Retrying {
                next_run_at: NOW + 1000 + 2000
            },
            FailOutcome::Dead
        ]
    );
    let f = fx.cat.read(move |c| files::file(c, file)).unwrap();
    assert_eq!(f.state, FileState::Dead);
    assert_eq!(f.error_code.as_deref(), Some("FileLocked"));
    // Dead jobs don't block "synced"; once scanned the folder is synced_with_errors.
    let st = fx
        .cat
        .write_blocking(move |c| {
            let scan = files::begin_scan(c, folder, now)?;
            files::mark_dir(c, scan, folder, "", true, None)?;
            files::observe(c, folder, scan, &obs("locked.docx", 1), now)?;
            files::finish_scan(c, folder, scan, now)?;
            status::refresh(c, folder)
        })
        .unwrap();
    assert_eq!(st.0, FolderStatus::SyncedWithErrors);
    // A new event for the dead file creates a fresh live job (dedupe only covers live jobs).
    let e = fx
        .cat
        .write_blocking(move |c| {
            jobs::enqueue(
                c,
                &NewJob::new(JobKind::IndexFile, Some(folder), Some(file), 1),
                now,
            )
        })
        .unwrap();
    assert!(!e.coalesced);
}

#[test]
fn non_retryable_folder_job_fails_and_folder_shows_error() {
    let fx = setup();
    let folder = add_folder(&fx.cat, fx.collection, &fx.root("err"));
    let epoch = fx.cat.start_epoch().unwrap().epoch;
    let (o, st) = fx
        .cat
        .write_blocking(move |c| {
            jobs::enqueue(
                c,
                &NewJob::new(JobKind::ScanFolder, Some(folder), None, 0),
                NOW,
            )?;
            let j = jobs::claim(c, &ClaimRequest::new(epoch, 1, NOW), &mut Drr::default())?;
            assert_eq!(status::refresh(c, folder)?.0, FolderStatus::Scanning);
            let o = jobs::fail(
                c,
                j[0].id,
                epoch,
                &failure(ErrorCode::Internal, false),
                &RetryPolicy::default(),
                NOW,
            )?;
            Ok((o, status::refresh(c, folder)?.0))
        })
        .unwrap();
    assert_eq!(o, FailOutcome::Failed);
    assert_eq!(st, FolderStatus::Error);
    let st = fx
        .cat
        .write_blocking(move |c| {
            assert_eq!(jobs::clear_terminal(c, folder, NOW)?, 1);
            status::refresh(c, folder)
        })
        .unwrap();
    assert_eq!(st.0, FolderStatus::Pending);
}

#[test]
fn claim_is_fair_across_folders_and_respects_priority_kinds_and_permits() {
    let fx = setup();
    let big = add_folder(&fx.cat, fx.collection, &fx.root("big"));
    let small = add_folder(&fx.cat, fx.collection, &fx.root("small"));
    let epoch = fx.cat.start_epoch().unwrap().epoch;
    fx.cat
        .write_blocking(move |c| {
            let scan_b = files::begin_scan(c, big, NOW)?;
            for i in 0..300u32 {
                let f = files::observe(c, big, scan_b, &obs(&format!("b{i}.md"), 0), NOW)?;
                let mut j = NewJob::new(JobKind::IndexFile, Some(big), Some(f.file_id), 2);
                j.run_at = Some(NOW - 1000 + i64::from(i)); // big folder's jobs are all older
                jobs::enqueue(c, &j, NOW)?;
            }
            let scan_s = files::begin_scan(c, small, NOW)?;
            for i in 0..5u32 {
                let f = files::observe(c, small, scan_s, &obs(&format!("s{i}.md"), 0), NOW)?;
                jobs::enqueue(
                    c,
                    &NewJob::new(JobKind::IndexFile, Some(small), Some(f.file_id), 2),
                    NOW,
                )?;
            }
            jobs::enqueue(c, &NewJob::new(JobKind::Retention, None, None, 3), NOW)?;
            jobs::enqueue(
                c,
                &NewJob::new(JobKind::ScanFolder, Some(small), None, 0),
                NOW,
            )?;
            Ok(())
        })
        .unwrap();
    let mut drr = Drr::default();
    let got = fx
        .cat
        .write_blocking(move |c| {
            let mut req = ClaimRequest::new(epoch, 9, NOW);
            req.aging_ms = 0;
            jobs::claim(c, &req, &mut drr)
        })
        .unwrap();
    assert_eq!(got.len(), 9, "permits cap the claim (backpressure)");
    assert_eq!(got[0].kind, JobKind::ScanFolder, "priority 0 first");
    let from_small = got.iter().filter(|j| j.folder_id == Some(small)).count();
    let from_big = got.iter().filter(|j| j.folder_id == Some(big)).count();
    assert_eq!(
        (from_small, from_big),
        (5, 4),
        "the small folder is not starved"
    );
    assert!(got.iter().all(|j| j.kind != JobKind::Retention));
    // Kind filter: only retention requested.
    let only = fx
        .cat
        .write_blocking(move |c| {
            let kinds = [JobKind::Retention];
            let mut req = ClaimRequest::new(epoch, 5, NOW);
            req.kinds = Some(&kinds);
            jobs::claim(c, &req, &mut Drr::default())
        })
        .unwrap();
    assert_eq!(only.len(), 1);
    assert_eq!(only[0].kind, JobKind::Retention);
    let (q, l) = fx.cat.read(jobs::queue_depth).unwrap();
    assert_eq!((q, l), (297, 10));
}

#[test]
fn aging_lifts_long_waiting_low_priority_jobs() {
    let fx = setup();
    let folder = add_folder(&fx.cat, fx.collection, &fx.root("age"));
    let epoch = fx.cat.start_epoch().unwrap().epoch;
    let old_id = fx
        .cat
        .write_blocking(move |c| {
            let old = jobs::enqueue(c, &NewJob::new(JobKind::Retention, None, None, 3), NOW)?;
            let scan = files::begin_scan(c, folder, NOW)?;
            for i in 0..5u32 {
                let f = files::observe(c, folder, scan, &obs(&format!("{i}.md"), 0), NOW)?;
                jobs::enqueue(
                    c,
                    &NewJob::new(JobKind::IndexFile, Some(folder), Some(f.file_id), 1),
                    NOW + 3 * jobs::DEFAULT_AGING_MS,
                )?;
            }
            Ok(old.id)
        })
        .unwrap();
    // After 3 aging periods the priority-3 job competes at priority 0 and is claimed first.
    let got = fx
        .cat
        .write_blocking(move |c| {
            jobs::claim(
                c,
                &ClaimRequest::new(epoch, 1, NOW + 3 * jobs::DEFAULT_AGING_MS),
                &mut Drr::default(),
            )
        })
        .unwrap();
    assert_eq!(got[0].id, old_id);
}

#[test]
fn dedupe_index_is_partial_and_history_is_pruned() {
    let fx = setup();
    let epoch = fx.cat.start_epoch().unwrap().epoch;
    fx.cat
        .write_blocking(move |c| {
            let a = jobs::enqueue(c, &NewJob::new(JobKind::Retention, None, None, 3), NOW)?;
            let j = jobs::claim(c, &ClaimRequest::new(epoch, 1, NOW), &mut Drr::default())?;
            assert_eq!(j[0].id, a.id);
            jobs::complete(c, a.id, epoch, NOW)?;
            let b = jobs::enqueue(c, &NewJob::new(JobKind::Retention, None, None, 3), NOW)?;
            assert!(!b.coalesced, "a finished job does not absorb new work");
            assert_eq!(
                jobs::prune_history(c, NOW + jobs::HISTORY_KEEP_MS + 1, jobs::HISTORY_KEEP_MS)?,
                1
            );
            Ok(())
        })
        .unwrap();
    let e = fx
        .cat
        .write_blocking(|c| jobs::enqueue(c, &NewJob::new(JobKind::Retention, None, None, 4), NOW))
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidInput);
}

/// CO-1: job ids are never reused once the queue drains (schema V2, `AUTOINCREMENT`), so every
/// finished job keeps its own `job_history` row with its own kind and payload. Under V1 the second
/// job got id 1 again and was archived as the first one.
#[test]
fn job_ids_are_never_reused_and_history_keeps_every_job() {
    let mut fx = setup();
    let folder = add_folder(&fx.cat, fx.collection, &fx.root("ids"));
    let file = add_file(&fx.cat, folder, "a.md", 1);
    let run = |cat: &sr_catalog::Catalog, payload: serde_json::Value| {
        let epoch = cat.start_epoch().unwrap().epoch;
        cat.write_blocking(move |c| {
            let mut j = NewJob::new(JobKind::IndexFile, Some(folder), Some(file), 1);
            j.payload = Some(payload);
            let e = jobs::enqueue(c, &j, NOW)?;
            let got = jobs::claim(c, &ClaimRequest::new(epoch, 1, NOW), &mut Drr::default())?;
            assert_eq!(got[0].id, e.id);
            assert_eq!(jobs::complete(c, e.id, epoch, NOW)?, Completed::Done);
            assert_eq!(jobs::queue_depth(c)?, (0, 0), "queue drained");
            Ok(e.id)
        })
        .unwrap()
    };
    let a = run(&fx.cat, serde_json::json!({ "n": 1 }));
    let b = run(&fx.cat, serde_json::json!({ "path_refresh": true }));
    fx.reopen(); // the sequence survives a restart
    let c = run(&fx.cat, serde_json::json!({ "n": 3 }));
    assert!(a < b && b < c, "ids reused: {a} {b} {c}");
    let hist: Vec<(i64, String)> = fx
        .cat
        .read(|c| {
            let mut st = c
                .prepare("SELECT id, payload FROM job_history ORDER BY id")
                .map_err(sr_catalog::map_db_err)?;
            let v = st
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(sr_catalog::map_db_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sr_catalog::map_db_err)?;
            Ok(v)
        })
        .unwrap();
    let payloads: Vec<serde_json::Value> = hist
        .iter()
        .map(|(_, p)| serde_json::from_str(p).unwrap())
        .collect();
    assert_eq!(
        payloads,
        vec![
            serde_json::json!({ "n": 1 }),
            serde_json::json!({ "path_refresh": true }),
            serde_json::json!({ "n": 3 }),
        ]
    );
}

/// CO-1: `ensure` creates a missing job but never touches a live one (no rerun of a running job,
/// no payload change), unlike `enqueue`.
#[test]
fn ensure_repairs_a_lost_job_without_rerunning_a_live_one() {
    let fx = setup();
    let folder = add_folder(&fx.cat, fx.collection, &fx.root("ensure"));
    let file = add_file(&fx.cat, folder, "a.md", 1);
    let epoch = fx.cat.start_epoch().unwrap().epoch;
    fx.cat
        .write_blocking(move |c| {
            let j = NewJob::new(JobKind::IndexFile, Some(folder), Some(file), 1);
            let first = jobs::ensure(c, &j, NOW)?;
            assert!(!first.coalesced, "lost job re-created");
            let again = jobs::ensure(c, &j, NOW)?;
            assert!(again.coalesced && again.id == first.id);
            let got = jobs::claim(c, &ClaimRequest::new(epoch, 1, NOW), &mut Drr::default())?;
            assert_eq!(got[0].id, first.id);
            let mut changed = j.clone();
            changed.payload = Some(serde_json::json!({ "force": true }));
            let leased = jobs::ensure(c, &changed, NOW)?;
            assert!(leased.coalesced && leased.id == first.id);
            assert_eq!(
                jobs::complete(c, first.id, epoch, NOW)?,
                Completed::Done,
                "no rerun"
            );
            assert_eq!(jobs::queue_depth(c)?, (0, 0));
            Ok(())
        })
        .unwrap();
}

/// CO-1: a path-only refresh (rename) and a content change for the same file coalesce into one
/// job that keeps both needs, in either order and while the first one runs.
#[test]
fn path_refresh_never_swallows_a_content_change() {
    use serde_json::json;
    let fx = setup();
    let folder = add_folder(&fx.cat, fx.collection, &fx.root("pr"));
    let a = add_file(&fx.cat, folder, "a.md", 1);
    let b = add_file(&fx.cat, folder, "b.md", 2);
    let c3 = add_file(&fx.cat, folder, "c.md", 3);
    let d = add_file(&fx.cat, folder, "d.md", 4);
    let epoch = fx.cat.start_epoch().unwrap().epoch;
    let pr = move || Some(json!({ jobs::PAYLOAD_PATH_REFRESH: true }));
    let job = move |f, p: Option<serde_json::Value>| {
        let mut j = NewJob::new(JobKind::IndexFile, Some(folder), Some(f), 1);
        j.payload = p;
        j
    };
    let live = fx
        .cat
        .write_blocking(move |c| {
            // a: rename first, then an edit.  b: edit first, then a rename.
            jobs::enqueue(c, &job(a, pr()), NOW)?;
            jobs::enqueue(c, &job(a, None), NOW)?;
            jobs::enqueue(c, &job(b, None), NOW)?;
            jobs::enqueue(c, &job(b, pr()), NOW)?;
            // c: two renames stay a cheap path-only refresh.
            jobs::enqueue(c, &job(c3, pr()), NOW)?;
            jobs::enqueue(c, &job(c3, pr()), NOW)?;
            // d: force is sticky.
            jobs::enqueue(c, &job(d, Some(json!({ jobs::PAYLOAD_FORCE: true }))), NOW)?;
            jobs::enqueue(c, &job(d, pr()), NOW)?;
            jobs::live_jobs(c, Some(folder))
        })
        .unwrap();
    let payload = |f| {
        live.iter()
            .find(|j| j.file_id == Some(f))
            .and_then(|j| j.payload.clone())
    };
    assert_eq!(live.len(), 4, "one live job per file");
    assert_eq!(payload(a), Some(json!({ "renamed": true })));
    assert_eq!(payload(b), Some(json!({ "renamed": true })));
    assert_eq!(payload(c3), Some(json!({ "path_refresh": true })));
    assert_eq!(payload(d), Some(json!({ "force": true, "renamed": true })));

    // While a path-only refresh runs, an edit arrives: the re-run is a full (renamed) job.
    let e = add_file(&fx.cat, folder, "e.md", 5);
    let rerun = fx
        .cat
        .write_blocking(move |c| {
            let mut drr = Drr::default();
            loop {
                let js = jobs::claim(c, &ClaimRequest::new(epoch, 16, NOW + 1), &mut drr)?;
                if js.is_empty() {
                    break;
                }
                for j in js {
                    jobs::complete(c, j.id, epoch, NOW + 1)?;
                }
            }
            jobs::enqueue(c, &job(e, pr()), NOW + 2)?;
            let leased = jobs::claim(c, &ClaimRequest::new(epoch, 16, NOW + 2), &mut drr)?;
            assert_eq!(leased.len(), 1);
            assert_eq!(leased[0].payload, pr());
            jobs::enqueue(c, &job(e, None), NOW + 3)?;
            assert_eq!(
                jobs::complete(c, leased[0].id, epoch, NOW + 4)?,
                Completed::Requeued
            );
            jobs::live_jobs(c, Some(folder))
        })
        .unwrap();
    assert_eq!(rerun.len(), 1);
    assert_eq!(rerun[0].payload, Some(json!({ "renamed": true })));
}

#[test]
fn merge_payload_rules() {
    use jobs::merge_payload as m;
    use serde_json::json;
    let pr = json!({ "path_refresh": true });
    let force = json!({ "force": true });
    assert_eq!(m(None, None), None);
    assert_eq!(m(Some(&pr), Some(&pr)), Some(pr.clone()));
    assert_eq!(m(None, Some(&pr)), Some(json!({ "renamed": true })));
    assert_eq!(m(Some(&pr), None), Some(json!({ "renamed": true })));
    assert_eq!(m(Some(&force), None), Some(force.clone()));
    assert_eq!(m(None, Some(&force)), Some(force.clone()));
    assert_eq!(
        m(
            Some(&json!({ "force": true, "path_refresh": true })),
            Some(&pr)
        ),
        Some(json!({ "force": true, "renamed": true })),
        "force wins over path-only"
    );
    // Generic keys: newer wins (scan triggers).
    assert_eq!(
        m(
            Some(&json!({ "trigger": "startup" })),
            Some(&json!({ "trigger": "manual" }))
        ),
        Some(json!({ "trigger": "manual" }))
    );
    assert_eq!(
        m(Some(&json!({ "trigger": "startup" })), None),
        Some(json!({ "trigger": "startup" }))
    );
    // Non-object payloads: newer wins, else older.
    assert_eq!(m(Some(&json!(1)), Some(&json!(2))), Some(json!(2)));
    assert_eq!(m(Some(&json!(1)), None), Some(json!(1)));
}
