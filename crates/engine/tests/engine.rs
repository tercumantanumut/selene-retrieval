//! sr-engine integration tests: start order, zero-config start, crash recovery (Amendment 4.2),
//! end-to-end indexing through the sandboxed worker, purge SLA with an injectable clock, rename
//! path refresh, events.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use sr_common::events::{EngineEvent, FolderStatus, HostState};
use sr_core::index::filter::Scope;
use sr_engine::clock::ManualClock;
use sr_engine::{Engine, ExtractSetup, SearchArgs};

fn args(q: &str) -> SearchArgs {
    SearchArgs {
        query: q.into(),
        max_results: Some(20),
        min_score: Some(0.0),
        folder_ids: None,
        mode: None,
        collapse_duplicates: None,
    }
}

const STEPS: &[&str] = &[
    "settings",
    "catalog_open",
    "start_epoch",
    "embedder",
    "open_stores",
    "sync_watermarks",
    "finalize_applied",
    "revalidate_roots",
    "tier_detection",
    "ready",
];

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zero_config_start_runs_steps_in_order() {
    let t = tmp();
    let data = t.path().join("d");
    assert!(!data.join("settings.toml").exists());
    let e = Engine::start(config(&data, ExtractSetup::Disabled))
        .await
        .unwrap();
    let r = e.startup_report();
    assert_eq!(r.steps, STEPS);
    assert!(r.settings_created, "settings.toml is created with defaults");
    assert!(data.join("settings.toml").is_file());
    assert_eq!(e.host_state(), HostState::Ready);
    let s = e.status();
    assert_eq!(s.value.summary, "No folders yet");
    let out = e.search(args("anything")).await.unwrap();
    assert!(out.no_folders && out.hits.is_empty());
    // Tier detection is recorded in catalog meta (silent).
    let det = e
        .catalog()
        .read(sr_catalog::repo::meta::hw_detection)
        .unwrap();
    assert!(det.is_some());
    // A second start bumps the epoch.
    let ep1 = e.epoch();
    e.drain().await;
    drop(e);
    let e2 = Engine::start(config(&data, ExtractSetup::Disabled))
        .await
        .unwrap();
    assert_eq!(e2.epoch(), ep1 + 1);
    assert!(!e2.startup_report().settings_created);
    e2.drain().await;
}

#[cfg(feature = "local")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bundled_model_starts_offline_with_no_settings() {
    let Some(app) = model_app_dir() else { return };
    let t = tmp();
    let data = t.path().join("d");
    let mut cfg = sr_engine::EngineConfig::new(data.clone(), app);
    cfg.extract = ExtractSetup::Disabled;
    cfg.background = false;
    let e = Engine::start(cfg).await.unwrap();
    assert_eq!(e.startup_report().steps, STEPS);
    let d = e.diagnostics();
    assert_eq!(d["embedder"]["model"], "BAAI/bge-small-en-v1.5");
    assert_eq!(d["embedder"]["dims"], 384);
    assert!(e.search(args("hello")).await.unwrap().no_folders);
    e.drain().await;
}

/// Index a small corpus through the real pipeline (sandboxed worker, chunker, T1, writer).
async fn indexed_engine(
    data: &std::path::Path,
    corpus: &std::path::Path,
    profile: &Profile,
    clock: Option<Arc<ManualClock>>,
) -> sr_engine::EngineHandle {
    let mut cfg = config(
        data,
        ExtractSetup::Config(Box::new(worker_config(data, profile))),
    );
    if let Some(c) = clock {
        cfg.clock = c;
    }
    let e = Engine::start(cfg).await.unwrap();
    e.add_folder(&corpus.to_string_lossy(), Some("Corpus".into()))
        .await
        .unwrap();
    e.run_pending_jobs().await.unwrap();
    e
}

fn corpus(root: &std::path::Path) {
    write_files(
        root,
        &[
            (
                "src/lib.rs",
                "//! demo\n\npub fn quixotic_frobnicator(x: u32) -> u32 {\n    x + 1\n}\n",
            ),
            (
                "docs/guide.md",
                "# Guide\n\nThe zanzibar protocol handles retries.\n\nMore text.\n",
            ),
            (
                "notes.txt",
                "line one\nline two mentions marmalade\nline three\n",
            ),
            (".env", "API_KEY=supersecretvalue123\n"),
            (
                "keys/server.pem",
                "-----BEGIN PRIVATE KEY----- privatepemvalue\n",
            ),
            (
                "node_modules/pkg/index.js",
                "module.exports = 'nodemodulestoken'\n",
            ),
            ("image.bin", "\u{0}\u{1}\u{2}binary"),
        ],
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn index_folder_end_to_end_and_never_index_secrets() {
    let t = tmp();
    let data = t.path().join("d");
    let root = t.path().join("corpus");
    corpus(&root);
    let p = Profile::new("e2e");
    let e = indexed_engine(&data, &root, &p, None).await;
    let f = e.list_folders().await.unwrap();
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].status, FolderStatus::Synced, "{f:?}");
    assert_eq!(f[0].files_indexed, 3, "lib.rs, guide.md, notes.txt: {f:?}");

    let out = e.search(args("quixotic_frobnicator")).await.unwrap();
    let hit = out.hits.first().expect("hit");
    assert_eq!(hit.rel_path, "src/lib.rs");
    assert!(hit.path.ends_with(r"corpus\src\lib.rs"), "{}", hit.path);
    assert!(hit.line_start >= 1 && hit.line_end >= hit.line_start);
    assert_eq!(hit.folder_name, "Corpus");

    let out = e.search(args("marmalade")).await.unwrap();
    assert_eq!(
        out.hits.first().map(|h| h.rel_path.as_str()),
        Some("notes.txt")
    );

    // Secrets and excluded dirs never reach the catalog or the stores.
    for q in ["supersecretvalue123", "privatepemvalue", "nodemodulestoken"] {
        let out = e.search(args(q)).await.unwrap();
        assert!(
            out.hits.iter().all(|h| !h.snippet.contains(q)),
            "{q} leaked: {:?}",
            out.hits
        );
    }
    assert!(
        !bytes_contain(&data, b"supersecretvalue123"),
        "secret bytes in data dir"
    );

    // Unchanged files are not re-extracted or re-embedded on a re-scan.
    e.sync_folder(None).await.unwrap();
    e.run_pending_jobs().await.unwrap();
    let f2 = e.list_folders().await.unwrap();
    assert_eq!(f2[0].files_indexed, 3);
    e.drain().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_after_store_commit_before_t2_is_finalized_at_restart() {
    let t = tmp();
    let data = t.path().join("d");
    let root = t.path().join("corpus");
    corpus(&root);
    let p = Profile::new("crash");
    let mut cfg = config(
        &data,
        ExtractSetup::Config(Box::new(worker_config(&data, &p))),
    );
    cfg.background = false;
    let e = Engine::start(cfg.clone()).await.unwrap();
    e.add_folder(&root.to_string_lossy(), None).await.unwrap();
    // T1 for every file, then a drain whose acknowledgement "crashes" after both stores committed.
    e.inject_ack_failures(1);
    assert!(
        e.run_pending_jobs().await.is_err(),
        "drain must fail at the injected crash"
    );
    let pending = e
        .catalog()
        .read(|c| {
            c.query_row(
                "SELECT count(*) FROM files WHERE state = 'pending_index'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map_err(sr_catalog::map_db_err)
        })
        .unwrap();
    assert_eq!(pending, 3, "files stay pending after the crash");
    let wm_before = e.store_watermarks().await.unwrap();
    assert!(
        wm_before.iter().all(|(_, s)| *s > 0),
        "stores committed: {wm_before:?}"
    );
    drop(e); // crash: no drain, no T2

    let e = Engine::start(cfg).await.unwrap();
    let r = e.startup_report();
    assert_eq!(r.replay_ops, 0, "nothing above the stores' watermarks");
    assert_eq!(
        r.finalized_files, 3,
        "finalize_applied cleared the pending files"
    );
    let f = e.list_folders().await.unwrap();
    assert_eq!(f[0].files_indexed, 3, "finalized before any job ran");
    // sr-sync's startup queued a catch-up scan (the folder shows `indexing`, never a premature
    // `synced`); it finds nothing changed.
    e.run_pending_jobs().await.unwrap();
    let f = e.list_folders().await.unwrap();
    assert_eq!(f[0].status, FolderStatus::Synced);
    let ops = e
        .catalog()
        .read(|c| {
            c.query_row("SELECT count(*) FROM index_ops", [], |r| r.get::<_, i64>(0))
                .map_err(sr_catalog::map_db_err)
        })
        .unwrap();
    assert_eq!(ops, 0, "outbox pruned");
    assert!(!e.search(args("zanzibar")).await.unwrap().hits.is_empty());
    e.drain().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t1_without_any_drain_is_replayed_at_restart() {
    // T1 written, the writer never ran (crash before the group commit): replay applies it at start.
    let t = tmp();
    let data = t.path().join("d");
    let root = t.path().join("corpus");
    corpus(&root);
    let p = Profile::new("replay");
    let cfg = config(
        &data,
        ExtractSetup::Config(Box::new(worker_config(&data, &p))),
    );
    let e = Engine::start(cfg.clone()).await.unwrap();
    e.add_folder(&root.to_string_lossy(), None).await.unwrap();
    e.run_jobs_only().await.unwrap();
    drop(e);
    let e = Engine::start(cfg).await.unwrap();
    assert!(
        e.startup_report().replay_ops > 0,
        "{:?}",
        e.startup_report()
    );
    // Amendment 4.24: nothing is replayed before ready; the status says so.
    let s = e.status();
    let cu = s.value.catch_up.expect("catching up");
    assert_eq!(cu.done, 0);
    assert!(
        s.value.summary.starts_with("Catching up after restart"),
        "{}",
        s.value.summary
    );
    assert_eq!(e.list_folders().await.unwrap()[0].files_indexed, 0);
    // The writer's first batches apply the backlog (the background writer does this itself).
    e.drain_writer_now().await.unwrap();
    assert!(e.status().value.catch_up.is_none());
    let f = e.list_folders().await.unwrap();
    assert_eq!(f[0].files_indexed, 3, "{:?}", e.startup_report());
    assert!(!e.search(args("marmalade")).await.unwrap().hits.is_empty());
    e.drain().await;
}

/// File names of the purge corpus (catalog `files.rel_path`).
const PURGE_NEEDLES: &[&[u8]] = &[b"docs/guide.md", b"notes.txt"];

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removed_folder_is_unsearchable_at_once_and_physically_gone_within_sla() {
    let t = tmp();
    let data = t.path().join("d");
    let root = t.path().join("corpus");
    corpus(&root);
    let clock = ManualClock::new();
    let p = Profile::new("purge");
    let e = indexed_engine(&data, &root, &p, Some(clock.clone())).await;
    let folder = e.list_folders().await.unwrap()[0].folder_id;
    let stores = e.registry().all();
    let coll = stores[0].clone();
    let scope = Scope {
        folders: Some(vec![folder]),
        ..Scope::collection(coll.collection_id)
    };
    let rows_before = coll.vectors.count(Some(&scope.filter())).await.unwrap();
    assert!(rows_before > 0);
    // A chunk id of this folder, to look for on disk afterwards.
    let chunk_hex: String = e
        .catalog()
        .read(|c| {
            c.query_row("SELECT hex(chunk_id) FROM chunks LIMIT 1", [], |r| {
                r.get::<_, String>(0)
            })
            .map_err(sr_catalog::map_db_err)
        })
        .unwrap()
        .to_ascii_lowercase();
    assert!(bytes_contain(&data.join("lance"), chunk_hex.as_bytes()));
    for needle in PURGE_NEEDLES {
        assert!(
            [data.join("catalog.db"), data.join("catalog.db-wal")]
                .iter()
                .any(|p| std::fs::read(p)
                    .is_ok_and(|b| b.windows(needle.len()).any(|w| w == *needle))),
            "precondition: the catalog holds {:?} before the purge",
            String::from_utf8_lossy(needle)
        );
    }

    let mut rx = e.bus().subscribe();
    e.remove_folder(folder).await.unwrap();
    // 1. Unsearchable immediately, before any store work.
    assert!(e.search(args("zanzibar")).await.unwrap().hits.is_empty());
    assert_eq!(
        coll.vectors.count(Some(&scope.filter())).await.unwrap(),
        rows_before,
        "stores untouched until the writer runs (no background writer in this test)"
    );
    // 2. Before the force threshold, maintenance leaves it to the group commit.
    clock.advance_ms(60_000);
    let m = e.run_maintenance_once().await.unwrap();
    assert_eq!(m.purges_forced, 0);
    // 3. Past 5 minutes the purge is forced: data leaves Lance, tantivy and the disk.
    clock.advance_ms(5 * 60_000);
    let m = e.run_maintenance_once().await.unwrap();
    assert_eq!(m.purges_forced, 1);
    assert_eq!(m.purges_completed, vec![folder]);
    assert_eq!(m.purge_sla_missed, 0);
    assert_eq!(coll.vectors.count(Some(&scope.filter())).await.unwrap(), 0);
    assert!(
        !bytes_contain(&data.join("lance"), chunk_hex.as_bytes()),
        "purged chunk id still on disk"
    );
    assert!(coll.lexical.all_chunk_ids().unwrap().is_empty());
    // R9: no catalog file keeps the purged names either (the WAL is checkpointed and truncated
    // once the purge completes; the database pages were rewritten by secure_delete).
    for name in ["catalog.db", "catalog.db-wal"] {
        let p = data.join(name);
        if !p.exists() {
            continue;
        }
        let bytes = std::fs::read(&p).unwrap();
        for needle in PURGE_NEEDLES {
            assert!(
                !bytes.windows(needle.len()).any(|w| w == *needle),
                "{name} still holds {:?} after the purge",
                String::from_utf8_lossy(needle)
            );
        }
    }
    let mut completed = false;
    while let Ok(ev) = rx.try_recv() {
        if ev.payload == (EngineEvent::PurgeCompleted { folder_id: folder }) {
            completed = true;
        }
    }
    assert!(completed, "PurgeCompleted published");
    // 4. Nothing left to purge: no SLA alarm later.
    clock.advance_ms(15 * 60_000);
    let m = e.run_maintenance_once().await.unwrap();
    assert_eq!((m.purges_forced, m.purge_sla_missed), (0, 0));
    e.drain().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn maintenance_optimizes_every_six_hours_and_prunes_versions() {
    let t = tmp();
    let data = t.path().join("d");
    let clock = ManualClock::new();
    let mut cfg = config(&data, ExtractSetup::Disabled);
    cfg.clock = clock.clone();
    let e = Engine::start(cfg).await.unwrap();
    let m = e.run_maintenance_once().await.unwrap();
    assert!(!m.optimized && !m.retention);
    clock.advance_ms(60 * 60_000);
    let m = e.run_maintenance_once().await.unwrap();
    assert!(m.retention && !m.optimized);
    clock.advance_ms(5 * 60 * 60_000);
    let m = e.run_maintenance_once().await.unwrap();
    assert!(m.optimized);
    e.drain().await;
}

/// CO-1 (closes m1-e O3 / W2-E O1): a rename found by a normal sync makes sr-sync enqueue
/// `index_file {"path_refresh": true}` through the dedupe-key path; once the jobs drain, the BM25
/// `path` field knows the new name and has forgotten the old one, and nothing was re-embedded
/// (same Lance rows, same chunk IDs, no embedder call).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rename_refreshes_path_tokens_without_reembedding() {
    use sr_common::ids::{ChunkId, FileId};

    let t = tmp();
    let data = t.path().join("d");
    let root = t.path().join("corpus");
    // Neither file name appears in the content, so only the path field can match them.
    write_files(
        &root,
        &[(
            "quokkaoldname.md",
            "# Notes\n\nplain content about gardens\n",
        )],
    );
    let p = Profile::new("rename");
    let embedder = hash_embedder();
    let mut cfg = config(
        &data,
        ExtractSetup::Config(Box::new(worker_config(&data, &p))),
    );
    cfg.embedder = sr_engine::EmbedderSource::Provided(embedder.clone());
    let e = Engine::start(cfg).await.unwrap();
    e.add_folder(&root.to_string_lossy(), Some("Corpus".into()))
        .await
        .unwrap();
    e.run_pending_jobs().await.unwrap();

    let lexical_hit = |out: &sr_engine::SearchOutput, name: &str| {
        out.hits
            .iter()
            .any(|h| h.rel_path == name && h.scores.bm25.is_some())
    };
    let hybrid = |q: &str| SearchArgs {
        mode: Some(sr_engine::SearchMode::Hybrid),
        ..args(q)
    };
    let before = e.search(hybrid("quokkaoldname")).await.unwrap();
    assert!(lexical_hit(&before, "quokkaoldname.md"), "{before:?}");
    let folder = before.hits[0].folder_id;

    let coll = e.registry().all()[0].clone();
    let lance_rows = || async {
        let mut r: Vec<(ChunkId, FileId)> = coll.vectors.rows(None).await.unwrap();
        r.sort();
        r
    };
    let catalog_chunks = || {
        e.catalog()
            .read(|c| {
                let mut st = c
                    .prepare("SELECT hex(chunk_id), hex(file_id) FROM chunks ORDER BY 1")
                    .map_err(sr_catalog::map_db_err)?;
                let v = st
                    .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                    .map_err(sr_catalog::map_db_err)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(sr_catalog::map_db_err)?;
                Ok(v)
            })
            .unwrap()
    };
    let rows_before = lance_rows().await;
    let chunks_before = catalog_chunks();
    assert!(!rows_before.is_empty());
    let embed_calls_before = embedder.calls().len();

    std::fs::rename(
        root.join("quokkaoldname.md"),
        root.join("zebrapathtoken.md"),
    )
    .unwrap();
    // A normal sync: sr-sync's reconciler sees the same NTFS id at a new path.
    e.sync_folder(Some(folder)).await.unwrap();
    e.run_pending_jobs().await.unwrap();

    // The refresh job ran through the dedupe-key path and is in the job history.
    let refreshed: i64 = e
        .catalog()
        .read(|c| {
            c.query_row(
                "SELECT count(*) FROM job_history WHERE kind = 'index_file' AND state = 'done'
                   AND json_extract(payload, '$.path_refresh') = 1",
                [],
                |r| r.get(0),
            )
            .map_err(sr_catalog::map_db_err)
        })
        .unwrap();
    assert_eq!(refreshed, 1, "one path_refresh job ran");
    let (q, l) = e
        .catalog()
        .read(sr_catalog::repo::jobs::queue_depth)
        .unwrap();
    assert_eq!(q + l, 0, "queue drained");

    // Nothing re-embedded: no embedder call, same Lance rows, same chunk IDs.
    assert_eq!(
        embedder.calls().len(),
        embed_calls_before,
        "rename must not re-embed"
    );
    assert_eq!(lance_rows().await, rows_before, "Lance rows unchanged");
    assert_eq!(catalog_chunks(), chunks_before, "chunk IDs unchanged");

    // New name found through the BM25 path field; the old name is gone from it.
    let new = e.search(hybrid("zebrapathtoken")).await.unwrap();
    let h = new.hits.first().expect("path token found after rename");
    assert_eq!(h.rel_path, "zebrapathtoken.md");
    assert!(lexical_hit(&new, "zebrapathtoken.md"), "{new:?}");
    assert_eq!(h.folder_id, folder);
    let old = e.search(hybrid("quokkaoldname")).await.unwrap();
    assert!(
        old.hits.iter().all(|h| h.scores.bm25.is_none()),
        "old name still matches lexically: {old:?}"
    );
    assert!(old.hits.iter().all(|h| h.rel_path != "quokkaoldname.md"));
    e.drain().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn events_are_pushed_for_folder_progress() {
    let t = tmp();
    let data = t.path().join("d");
    let root = t.path().join("corpus");
    corpus(&root);
    let p = Profile::new("events");
    let e = Engine::start(config(
        &data,
        ExtractSetup::Config(Box::new(worker_config(&data, &p))),
    ))
    .await
    .unwrap();
    let mut rx = e.bus().subscribe();
    e.add_folder(&root.to_string_lossy(), None).await.unwrap();
    e.run_pending_jobs().await.unwrap();
    let mut seen = Vec::new();
    while let Ok(ev) = tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
        let Ok(ev) = ev else { break };
        seen.push(ev);
    }
    let seqs: Vec<u64> = seen.iter().map(|e| e.seq).collect();
    assert!(
        seqs.windows(2).all(|w| w[1] == w[0] + 1),
        "gap-free: {seqs:?}"
    );
    let kinds: Vec<&EngineEvent> = seen.iter().map(|e| &e.payload).collect();
    assert!(
        kinds
            .iter()
            .any(|k| matches!(k, EngineEvent::ScanProgress { .. }))
    );
    assert!(
        kinds
            .iter()
            .any(|k| matches!(k, EngineEvent::IndexCommitted { .. }))
    );
    assert!(kinds.iter().any(|k| matches!(
        k,
        EngineEvent::FolderStatusChanged {
            status: FolderStatus::Synced,
            ..
        }
    )));
    e.drain().await;
}

/// M1 tester regression: when an edit is finished by the writer's commit (T2), the last pushed
/// folder status must be `synced` again (sr-sync had pushed `indexing` meanwhile), and the folder
/// records its success time. Before the fix the engine compared only with its own last publish,
/// stayed silent, and `sync_folder` with a progress token waited for its 10-minute cap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn edit_finished_by_the_writer_pushes_synced_again() {
    let t = tmp();
    let data = t.path().join("d");
    let root = t.path().join("corpus");
    corpus(&root);
    let p = Profile::new("resync");
    let e = Engine::start(config(
        &data,
        ExtractSetup::Config(Box::new(worker_config(&data, &p))),
    ))
    .await
    .unwrap();
    e.add_folder(&root.to_string_lossy(), None).await.unwrap();
    e.run_pending_jobs().await.unwrap();
    let folder = e.list_folders().await.unwrap()[0].folder_id;
    let last_status = |seen: &[sr_common::events::Seq<EngineEvent>]| {
        seen.iter().rev().find_map(|ev| match &ev.payload {
            EngineEvent::FolderStatusChanged {
                folder_id, status, ..
            } if *folder_id == folder => Some(*status),
            _ => None,
        })
    };
    for round in 0..2 {
        let mut rx = e.bus().subscribe();
        std::fs::write(
            root.join("notes.txt"),
            format!("line one\nedited in round {round} with porcupinetoken{round}\n"),
        )
        .unwrap();
        e.sync_folder(Some(folder)).await.unwrap();
        e.run_pending_jobs().await.unwrap();
        let mut seen = Vec::new();
        while let Ok(ev) = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
            let Ok(ev) = ev else { break };
            seen.push(ev);
        }
        let pushed: Vec<_> = seen
            .iter()
            .filter_map(|ev| match &ev.payload {
                EngineEvent::FolderStatusChanged { status, .. } => Some(*status),
                _ => None,
            })
            .collect();
        assert!(
            pushed.iter().any(|s| *s != FolderStatus::Synced),
            "round {round}: the edit was announced: {pushed:?}"
        );
        assert_eq!(
            last_status(&seen),
            Some(FolderStatus::Synced),
            "round {round}: last pushed status: {pushed:?}"
        );
    }
    let info = e.list_folders().await.unwrap();
    assert!(info[0].last_success_at.is_some(), "{info:?}");
    e.drain().await;
}

/// `n` small files, each one chunk of unique text (`uniqword<i>`), below `root/<dir>`.
fn many_files(root: &std::path::Path, dir: &str, from: usize, n: usize) {
    for i in from..from + n {
        let p = root.join(dir).join(format!("note{i:04}.md"));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(
            p,
            format!("# Note {i}\n\nuniqword{i} appears only in note {i} of the {dir} set.\n"),
        )
        .unwrap();
    }
}

fn outbox_len(e: &sr_engine::EngineHandle) -> i64 {
    e.catalog()
        .read(|c| {
            c.query_row("SELECT count(*) FROM index_ops", [], |r| r.get::<_, i64>(0))
                .map_err(sr_catalog::map_db_err)
        })
        .unwrap()
}

/// Amendment 4.24 (M1 tester item B): a restart with a large pending outbox is ready at once.
/// The start returns (the daemon opens its pipe right after) in well under 2 s although the
/// backlog needs many seconds of embedding; search serves the last committed state meanwhile;
/// the status shows "Catching up after restart" with pushed progress; the background replay
/// converges (stores == catalog).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_with_a_large_pending_outbox_is_ready_at_once_then_catches_up() {
    let t = tmp();
    let data = t.path().join("d");
    let root = t.path().join("corpus");
    corpus(&root);
    let p = Profile::new("catchup");
    let wc = worker_config(&data, &p);
    // Run 1 (fast embedder): the base corpus is committed; then 150 new files reach T1 only.
    let cfg = config(&data, ExtractSetup::Config(Box::new(wc.clone())));
    let e = Engine::start(cfg).await.unwrap();
    e.add_folder(&root.to_string_lossy(), None).await.unwrap();
    e.run_pending_jobs().await.unwrap();
    many_files(&root, "late", 0, 150);
    e.sync_folder(None).await.unwrap();
    e.run_jobs_only().await.unwrap();
    assert!(outbox_len(&e) >= 150);
    drop(e); // crash: the writer never ran for the new files

    // Run 2: 150 texts x 40 ms = 6 s of embedding pending; the start must not wait for it.
    let slow = SlowEmbedder::new(Duration::from_millis(40), 8);
    let t0 = std::time::Instant::now();
    let e = Engine::start(slow_config(
        &data,
        ExtractSetup::Config(Box::new(wc)),
        slow.clone(),
    ))
    .await
    .unwrap();
    let ready_after = t0.elapsed();
    let mut rx = e.bus().subscribe();
    assert!(
        ready_after < Duration::from_secs(2),
        "ready after {ready_after:?}"
    );
    assert_eq!(e.host_state(), HostState::Ready);
    assert!(e.startup_report().replay_ops >= 150);
    let s = e.status();
    let cu = s.value.catch_up.expect("catching up after restart");
    assert!(cu.total >= 150 && cu.done < cu.total, "{cu:?}");
    assert!(
        s.value.summary.starts_with("Catching up after restart"),
        "{}",
        s.value.summary
    );
    // Search serves the last committed state right away.
    let t1 = std::time::Instant::now();
    let hits = e.search(args("zanzibar")).await.unwrap().hits;
    assert!(
        !hits.is_empty(),
        "committed files are searchable during catch-up"
    );
    assert!(
        t1.elapsed() < Duration::from_secs(2),
        "search is not blocked by the writer"
    );
    assert!(status_text(&e).contains("Search works meanwhile"));

    // Progress is pushed; the phase ends with done == total.
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    let mut progress = Vec::new();
    while std::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
            Ok(Ok(ev)) => {
                if let EngineEvent::CatchUpProgress { done, total } = ev.payload {
                    progress.push((done, total));
                    if done == total {
                        break;
                    }
                }
            }
            _ => {
                if e.status().value.catch_up.is_none() {
                    break;
                }
            }
        }
    }
    assert!(e.status().value.catch_up.is_none(), "catch-up finished");
    assert!(
        progress.len() >= 2,
        "progress pushed per batch: {progress:?}"
    );
    assert!(
        progress.windows(2).all(|w| w[0].0 <= w[1].0),
        "{progress:?}"
    );
    assert_eq!(progress.last().map(|p| p.0 == p.1), Some(true));
    assert!(e.wait_idle(Duration::from_secs(120)).await);
    let (cat, lance, lance_rows, lex, lex_docs) = store_counts(&e).await;
    assert_eq!((lance, lance_rows, lex, lex_docs), (cat, cat, cat, cat));
    assert!(!e.search(args("uniqword149")).await.unwrap().hits.is_empty());
    assert_eq!(outbox_len(&e), 0);
    e.drain().await;
}

/// Plain status text through the public renderer.
fn status_text(e: &sr_engine::EngineHandle) -> String {
    sr_engine::EngineHandle::status_text(&e.status().value)
}

/// Amendment 4.25 (M1 tester item C): with a CPU-speed embedder and a large backlog, commits are
/// bounded (progress pushed every few seconds, not once per 1 000 files); the shutdown drain
/// returns within 10 s and leaves the rest in the outbox; the next start applies exactly the
/// remainder (committed vectors are reused, nothing is duplicated).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bounded_batches_push_progress_and_shutdown_drain_is_quick() {
    let t = tmp();
    let data = t.path().join("d");
    let root = t.path().join("corpus");
    many_files(&root, "big", 0, 600);
    let p = Profile::new("bounded");
    let wc = worker_config(&data, &p);
    // 600 texts x 30 ms = 18 s of embedding: far more than one batch or the drain budget.
    let slow = SlowEmbedder::new(Duration::from_millis(30), 8);
    let e = Engine::start(slow_config(
        &data,
        ExtractSetup::Config(Box::new(wc.clone())),
        slow.clone(),
    ))
    .await
    .unwrap();
    let mut rx = e.bus().subscribe();
    e.add_folder(&root.to_string_lossy(), None).await.unwrap();

    // Watch commits for a while: IndexProgress at least every few seconds.
    let mut last = std::time::Instant::now();
    let mut seen_first = false;
    let mut max_gap = Duration::ZERO;
    let mut commits = 0;
    let watch_until = std::time::Instant::now() + Duration::from_secs(60);
    while std::time::Instant::now() < watch_until && commits < 4 {
        let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_secs(15), rx.recv()).await else {
            panic!("no event for 15 s");
        };
        if let EngineEvent::IndexProgress { files_done, .. } = ev.payload
            && files_done > 0
        {
            let now = std::time::Instant::now();
            if seen_first {
                max_gap = max_gap.max(now - last);
            }
            seen_first = true;
            last = now;
            commits += 1;
        }
    }
    assert!(commits >= 4, "only {commits} progress events");
    assert!(
        max_gap < Duration::from_secs(5),
        "progress gap {max_gap:?} (bounded batches close at 2 s)"
    );
    let done_before = e.list_folders().await.unwrap()[0].files_indexed;
    assert!(done_before < 600, "backlog left for the drain");

    // Shutdown drain: <= 10 s, the rest stays in the outbox.
    let t0 = std::time::Instant::now();
    e.drain().await;
    let took = t0.elapsed();
    assert!(took <= Duration::from_secs(10), "drain took {took:?}");
    assert!(outbox_len(&e) > 0, "the rest stays in the outbox");
    let committed = store_counts(&e).await.1;
    assert!(committed < 600);
    let embedded_run1 = slow.embedded();
    drop(e);

    // Restart: exactly the remainder is embedded; stores converge with no duplicates.
    let slow2 = SlowEmbedder::new(Duration::ZERO, 64);
    let e = Engine::start(slow_config(
        &data,
        ExtractSetup::Config(Box::new(wc)),
        slow2.clone(),
    ))
    .await
    .unwrap();
    assert!(e.wait_idle(Duration::from_secs(180)).await);
    let (cat, lance, lance_rows, lex, lex_docs) = store_counts(&e).await;
    assert_eq!(cat, 600);
    assert_eq!((lance, lance_rows, lex, lex_docs), (cat, cat, cat, cat));
    assert_eq!(
        slow2.embedded(),
        600 - committed,
        "committed vectors are reused (run 1 embedded {embedded_run1})"
    );
    assert_eq!(e.list_folders().await.unwrap()[0].files_indexed, 600);
    e.drain().await;
}

/// R-UX19: a settings value that had to be reset reaches clients: snapshot `notices`, a
/// `Notice` event, and the plain status text (`sr://status`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settings_reset_notice_reaches_clients() {
    let t = tmp();
    let data = t.path().join("d");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(
        data.join("settings.toml"),
        "[search]\ntier_override = \"sometimes\"\n",
    )
    .unwrap();
    let e = Engine::start(config(&data, ExtractSetup::Disabled))
        .await
        .unwrap();
    assert!(e.startup_report().settings_reset_fields >= 1);
    let s = e.status().value;
    assert_eq!(s.notices.len(), 1);
    let n = &s.notices[0];
    assert_eq!(n.code, sr_common::error::ErrorCode::SettingsInvalid);
    assert_eq!(n.message.title, "Some settings were reset");
    assert!(
        n.fields.iter().any(|f| f.contains("tier_override")),
        "{:?}",
        n.fields
    );
    let text = sr_engine::EngineHandle::status_text(&s);
    assert!(
        text.contains("Note: Some settings were reset. A few settings had invalid values"),
        "{text}"
    );
    let snap = serde_json::to_value(&s).unwrap();
    assert_eq!(
        snap["notices"][0]["message"]["title"],
        "Some settings were reset"
    );
    e.drain().await;
}

/// Live sync (M2-W watcher wired at engine start): a file saved into a folder becomes searchable
/// without any `sync_folder` call.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn saved_file_is_picked_up_by_the_live_watcher() {
    let t = tmp();
    let data = t.path().join("d");
    let root = t.path().join("corpus");
    corpus(&root);
    let p = Profile::new("watch");
    let mut cfg = config(
        &data,
        ExtractSetup::Config(Box::new(worker_config(&data, &p))),
    );
    cfg.background = true;
    let e = Engine::start(cfg).await.unwrap();
    e.add_folder(&root.to_string_lossy(), None).await.unwrap();
    assert!(e.wait_idle(Duration::from_secs(120)).await);
    std::fs::write(
        root.join("docs").join("fresh.md"),
        "# Fresh\n\nThe watcherpickedtoken shows live sync works.\n",
    )
    .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let hits = e.search(args("watcherpickedtoken")).await.unwrap().hits;
        if hits.iter().any(|h| h.rel_path == "docs/fresh.md") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "saved file not searchable within 30 s"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let t0 = std::time::Instant::now();
    e.drain().await;
    assert!(t0.elapsed() < Duration::from_secs(10));
}

/// docs/06 change 5: a JSON data dump (lines averaging over 1,000 bytes) is skipped as
/// `data_file` (with its reason in the skipped list); normal JSON is indexed, and a
/// `.seleneignore` `!pattern` turns the rule off for that file.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn long_line_json_is_skipped_as_a_data_file_unless_re_included() {
    let t = tmp();
    let data = t.path().join("d");
    let root = t.path().join("corpus");
    let row = |w: &str| format!("{{\"q\": \"{w}\", \"pad\": \"{}\"}}\n", "x".repeat(1200));
    let dump = format!("{}{}", row("dumpcanarytoken"), row("second"));
    let kept = row("keptcanarytoken");
    write_files(
        &root,
        &[
            ("baselines/run.json", dump.as_str()),
            ("fixtures/kept.json", kept.as_str()),
            ("config.json", "{\n  \"name\": \"configcanarytoken\"\n}\n"),
            (".seleneignore", "!fixtures/kept.json\n"),
        ],
    );
    let p = Profile::new("datafile");
    let e = indexed_engine(&data, &root, &p, None).await;
    let f = e.list_folders().await.unwrap();
    assert_eq!(f[0].files_indexed, 2, "{f:?}");
    let hit = |q: &'static str| {
        let e = e.clone();
        async move {
            e.search(args(q))
                .await
                .unwrap()
                .hits
                .iter()
                .any(|h| h.snippet.contains(q))
        }
    };
    assert!(
        !hit("dumpcanarytoken").await,
        "the data dump is not indexed"
    );
    assert!(hit("keptcanarytoken").await, "re-included by .seleneignore");
    assert!(hit("configcanarytoken").await, "normal JSON is indexed");
    let skipped = e
        .catalog()
        .read(move |c| {
            sr_catalog::repo::files::files_in_state(
                c,
                f[0].folder_id,
                sr_catalog::FileState::Skipped,
            )
        })
        .unwrap();
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0].rel_path, "baselines/run.json");
    assert_eq!(skipped[0].skip_reason.as_deref(), Some("data_file"));
    e.drain().await;
}
