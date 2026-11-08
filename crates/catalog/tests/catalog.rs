//! sr-catalog integration tests: schema, identity, outbox, mark-and-sweep, profiles, read pool.
// Test crate: helpers outside #[test] fns may unwrap (clippy.toml covers only #[test] fns).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use sr_catalog::repo::chunks;
use sr_catalog::repo::files;
use sr_catalog::repo::folders;
use sr_catalog::repo::meta;
use sr_catalog::repo::outbox::{self, CoalescedOp};
use sr_catalog::repo::profiles::{self, ProfileSpec};
use sr_catalog::repo::status;
use sr_catalog::{Catalog, IndexState, Store, schema};
use sr_common::error::ErrorCode;
use sr_common::events::FolderStatus;
use sr_common::ids::ChunkId;
use sr_common::tier::STANDARD;

// --------------------------------------------------------------------------------- migrations

#[test]
fn migrates_from_empty_and_reopens_idempotently() {
    let mut fx = setup();
    assert_eq!(
        fx.cat.schema_version().unwrap(),
        i64::try_from(schema::LATEST_VERSION).unwrap()
    );
    schema::migrations().validate().unwrap();
    let tables: Vec<String> = fx
        .cat
        .read(|c| {
            let mut st = c
                .prepare("SELECT name FROM sqlite_master WHERE type IN ('table') ORDER BY name")
                .map_err(sr_catalog::map_db_err)?;
            let v = st
                .query_map([], |r| r.get(0))
                .map_err(sr_catalog::map_db_err)?
                .collect::<rusqlite::Result<Vec<String>>>()
                .map_err(sr_catalog::map_db_err)?;
            Ok(v)
        })
        .unwrap();
    for t in [
        "meta",
        "collections",
        "embedding_profiles",
        "vector_indexes",
        "model_files",
        "folders",
        "scan_dirs",
        "files",
        "files_fts",
        "chunks",
        "chunk_text",
        "index_ops",
        "store_watermarks",
        "jobs",
        "job_history",
        "approvals",
        "clients",
        "egress_consent",
        "egress_audit",
    ] {
        assert!(
            tables.iter().any(|x| x == t),
            "missing table {t}: {tables:?}"
        );
    }
    // Pragmas (M3).
    let (jm, fk, av): (String, i64, i64) = fx
        .cat
        .read(|c| {
            Ok((
                c.query_row("PRAGMA journal_mode", [], |r| r.get(0))
                    .map_err(sr_catalog::map_db_err)?,
                c.query_row("PRAGMA foreign_keys", [], |r| r.get(0))
                    .map_err(sr_catalog::map_db_err)?,
                c.query_row("PRAGMA auto_vacuum", [], |r| r.get(0))
                    .map_err(sr_catalog::map_db_err)?,
            ))
        })
        .unwrap();
    assert_eq!((jm.as_str(), fk, av), ("wal", 1, 2));
    fx.reopen();
    assert_eq!(
        fx.cat.schema_version().unwrap(),
        i64::try_from(schema::LATEST_VERSION).unwrap()
    );
    assert_eq!(fx.cat.readers(), 3);
}

#[test]
fn read_connections_cannot_write() {
    let fx = setup();
    let e = fx
        .cat
        .read(|c| {
            c.execute("INSERT INTO meta(key, value) VALUES ('x', 'y')", [])
                .map_err(sr_catalog::map_db_err)
        })
        .unwrap_err();
    assert_ne!(e.code, ErrorCode::NotFound);
}

// --------------------------------------------------------------------------------- identity (D2)

#[test]
fn byte_identical_files_in_two_folders_and_collections_get_distinct_chunk_ids() {
    let fx = setup();
    let other_col = fx
        .cat
        .write_blocking({
            let p = fx.profile;
            move |c| profiles::create_collection(c, "Work", p, false, NOW)
        })
        .unwrap();
    let fa = add_folder(&fx.cat, fx.collection, &fx.root("a"));
    let fb = add_folder(&fx.cat, fx.collection, &fx.root("b"));
    let fc = add_folder(&fx.cat, other_col, &fx.root("a")); // same root, other collection: allowed
    let content = "Apache License\nVersion 2.0\nSame bytes everywhere";
    let ids: Vec<Vec<ChunkId>> = [fa, fb, fc]
        .into_iter()
        .map(|f| {
            let file = add_file(&fx.cat, f, "LICENSE", 1);
            t1(&fx.cat, file, content).chunk_ids
        })
        .collect();
    let all: HashSet<ChunkId> = ids.iter().flatten().copied().collect();
    assert_eq!(all.len(), 9, "3 files x 3 chunks, all distinct");
    // text_hash dedupe: the text is stored once, referenced three times.
    let (texts, refs): (i64, i64) = fx
        .cat
        .read(|c| {
            c.query_row(
                "SELECT count(*), coalesce(sum(refcount), 0) FROM chunk_text",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(sr_catalog::map_db_err)
        })
        .unwrap();
    assert_eq!((texts, refs), (3, 9));
}

#[test]
fn chunk_ids_are_stable_across_rename() {
    let fx = setup();
    let folder = add_folder(&fx.cat, fx.collection, &fx.root("r"));
    let file = add_file(&fx.cat, folder, "docs/old-name.md", 9);
    let content = "alpha\nbeta";
    let before = t1(&fx.cat, file, content).chunk_ids;
    // Next scan sees the same NTFS identity at a new path.
    let o = obs("docs/new-name.md", 9);
    let seen = fx
        .cat
        .write_blocking(move |c| {
            let scan = files::begin_scan(c, folder, NOW + 1)?;
            files::mark_dir(c, scan, folder, "", true, None)?;
            files::mark_dir(c, scan, folder, "docs", true, None)?;
            let s = files::observe(c, folder, scan, &o, NOW + 1)?;
            let sweep = files::finish_scan(c, folder, scan, NOW + 1)?;
            Ok((s, sweep))
        })
        .unwrap();
    assert_eq!(seen.0.file_id, file, "file_id survives the rename");
    assert_eq!(seen.0.renamed_from.as_deref(), Some("docs/old-name.md"));
    assert!(seen.1.deleted.is_empty(), "rename is not delete+add");
    let after = t1(&fx.cat, file, content).chunk_ids;
    assert_eq!(before, after);
    // Path search follows the rename (FTS triggers).
    let hits = fx
        .cat
        .read(|c| files::find_files(c, "new-name", 10))
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert!(
        fx.cat
            .read(|c| files::find_files(c, "old", 10))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn hard_link_seen_twice_in_one_scan_is_two_files() {
    let fx = setup();
    let folder = add_folder(&fx.cat, fx.collection, &fx.root("h"));
    let (a, b) = fx
        .cat
        .write_blocking(move |c| {
            let scan = files::begin_scan(c, folder, NOW)?;
            let a = files::observe(c, folder, scan, &obs("a.txt", 3), NOW)?;
            let b = files::observe(c, folder, scan, &obs("b.txt", 3), NOW)?;
            Ok((a, b))
        })
        .unwrap();
    assert!(a.new && b.new);
    assert_ne!(a.file_id, b.file_id);
}

// --------------------------------------------------------------------------------- outbox (D3)

#[test]
fn outbox_coalesces_per_file_and_resolves_missing_files_to_delete() {
    let fx = setup();
    let folder = add_folder(&fx.cat, fx.collection, &fx.root("o"));
    let f1 = add_file(&fx.cat, folder, "1.md", 1);
    let f2 = add_file(&fx.cat, folder, "2.md", 2);
    let f3 = add_file(&fx.cat, folder, "3.md", 3);
    t1(&fx.cat, f1, "v1");
    t1(&fx.cat, f1, "v2");
    let last1 = t1(&fx.cat, f1, "v3");
    t1(&fx.cat, f2, "x");
    t1(&fx.cat, f3, "y");
    fx.cat
        .write_blocking(move |c| files::delete_file(c, f3, NOW))
        .unwrap();
    let col = fx.collection;
    let batch = fx
        .cat
        .read(move |c| outbox::ops_after(c, col, 0, 100))
        .unwrap();
    assert_eq!(batch.raw, 6);
    assert_eq!(batch.ops.len(), 3, "{:?}", batch.ops);
    assert!(batch.ops.contains(&(
        last1.seq,
        CoalescedOp::Upsert {
            file_id: f1,
            file_version: 3
        }
    )));
    assert!(
        batch
            .ops
            .iter()
            .any(|(_, o)| *o == CoalescedOp::Delete { file_id: f3 })
    );
    assert!(
        batch.ops.windows(2).all(|w| w[0].0 < w[1].0),
        "ordered by seq"
    );
    // Paging: through_seq covers coalesced-away rows.
    let page = fx
        .cat
        .read(move |c| outbox::ops_after(c, col, 0, 2))
        .unwrap();
    assert_eq!(page.raw, 2);
    assert_eq!(page.ops.len(), 1);
    let rest = fx
        .cat
        .read(move |c| outbox::ops_after(c, col, page.through_seq, 100))
        .unwrap();
    assert_eq!(rest.raw, 4);
    // A file removed after its upsert was written replays as a delete (never resurrected).
    let f4 = add_file(&fx.cat, folder, "4.md", 4);
    let s4 = t1(&fx.cat, f4, "z").seq;
    fx.cat
        .write_blocking(move |c| {
            c.execute("DELETE FROM files WHERE id = ?1", [f4.to_bytes()])
                .map_err(sr_catalog::map_db_err)?;
            Ok(())
        })
        .unwrap();
    let b = fx
        .cat
        .read(move |c| outbox::ops_after(c, col, s4 - 1, 10))
        .unwrap();
    assert_eq!(b.ops, vec![(s4, CoalescedOp::Delete { file_id: f4 })]);
}

#[test]
fn t2_finalizes_only_current_version_and_prune_respects_every_watermark() {
    let fx = setup();
    let folder = add_folder(&fx.cat, fx.collection, &fx.root("t"));
    let f = add_file(&fx.cat, folder, "a.md", 1);
    let o1 = t1(&fx.cat, f, "one");
    let o2 = t1(&fx.cat, f, "two");
    let fin_old = fx
        .cat
        .write_blocking(move |c| chunks::finalize_file(c, f, o1.file_version, NOW))
        .unwrap();
    assert!(!fin_old, "stale T2 must not mark a newer version indexed");
    assert!(
        fx.cat
            .write_blocking(move |c| chunks::finalize_file(c, f, o2.file_version, NOW))
            .unwrap()
    );
    let col = fx.collection;
    let idx = fx
        .cat
        .read(move |c| profiles::active_index(c, col))
        .unwrap()
        .unwrap()
        .0;
    // Only tantivy caught up: nothing pruned.
    let pruned = fx
        .cat
        .write_blocking(move |c| {
            outbox::set_watermark(c, Store::Tantivy, &col.to_bytes(), o2.seq)?;
            outbox::prune(c)
        })
        .unwrap();
    assert_eq!(pruned, 0);
    let pruned = fx
        .cat
        .write_blocking(move |c| {
            outbox::set_watermark(c, Store::Lance, &idx.id.to_bytes(), o1.seq)?;
            outbox::prune(c)
        })
        .unwrap();
    assert_eq!(pruned, 1);
    // Watermarks never go backwards.
    let wm = fx
        .cat
        .write_blocking(move |c| {
            outbox::set_watermark(c, Store::Lance, &idx.id.to_bytes(), 0)?;
            outbox::watermark(c, Store::Lance, &idx.id.to_bytes())
        })
        .unwrap();
    assert_eq!(wm, o1.seq);
    // Seq is never reused after pruning everything.
    let f2 = add_file(&fx.cat, folder, "b.md", 2);
    fx.cat
        .write_blocking(move |c| {
            outbox::set_watermark(c, Store::Lance, &idx.id.to_bytes(), o2.seq)?;
            outbox::prune(c)
        })
        .unwrap();
    assert!(t1(&fx.cat, f2, "b").seq > o2.seq);
}

#[test]
fn chunk_text_round_trips_and_hydrates() {
    let fx = setup();
    let folder = add_folder(&fx.cat, fx.collection, &fx.root("h"));
    let f = add_file(&fx.cat, folder, "src/lib.rs", 1);
    let o = t1(&fx.cat, f, "fn main() {}\n// ünïcödé ✓");
    let ids = o.chunk_ids.clone();
    let hits = fx.cat.read(move |c| chunks::hydrate(c, &ids)).unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[1].text, "// ünïcödé ✓");
    assert_eq!(hits[1].rel_path, "src/lib.rs");
    assert_eq!(hits[1].line_start, 2);
    // After an edit the old ids are stale and filtered out at query time.
    let old = o.chunk_ids.clone();
    t1(&fx.cat, f, "fn main() { changed }");
    let cur = fx.cat.read(move |c| chunks::hydrate(c, &old)).unwrap();
    assert!(cur.is_empty());
    // Batched lookups keep the caller's order across batch boundaries, skipping unknown ids.
    let o = t1(&fx.cat, f, "a\nb");
    let mut ids: Vec<ChunkId> = (0..chunks::HYDRATE_BATCH as u32 + 7)
        .map(|i| {
            let mut b = [0xEE; 32];
            b[..4].copy_from_slice(&i.to_le_bytes());
            ChunkId::from_bytes(b)
        })
        .collect();
    ids.insert(3, o.chunk_ids[1]);
    ids.push(o.chunk_ids[0]);
    let hits = fx.cat.read(move |c| chunks::hydrate(c, &ids)).unwrap();
    assert_eq!(
        hits.iter().map(|h| h.text.as_str()).collect::<Vec<_>>(),
        vec!["b", "a"]
    );
}

// --------------------------------------------------------------------------------- mark and sweep

#[test]
fn sweep_deletes_only_under_completely_listed_directories() {
    let fx = setup();
    let folder = add_folder(&fx.cat, fx.collection, &fx.root("s"));
    for (i, p) in [
        "keep/a.md",
        "locked/b.md",
        "gone/c.md",
        "top.md",
        "gone/deep/d.md",
    ]
    .iter()
    .enumerate()
    {
        add_file(&fx.cat, folder, p, u8::try_from(10 + i).unwrap());
    }
    // New scan: "locked" could not be listed; "gone" no longer exists (root listed completely);
    // nothing was re-seen except keep/a.md.
    let res = fx
        .cat
        .write_blocking(move |c| {
            let scan = files::begin_scan(c, folder, NOW + 5)?;
            files::mark_dir(c, scan, folder, "", true, None)?;
            files::mark_dir(c, scan, folder, "keep", true, None)?;
            files::mark_dir(
                c,
                scan,
                folder,
                "locked",
                false,
                Some(ErrorCode::PermissionDenied),
            )?;
            files::observe(c, folder, scan, &obs("keep/a.md", 10), NOW + 5)?;
            files::finish_scan(c, folder, scan, NOW + 5)
        })
        .unwrap();
    assert!(res.partial);
    assert_eq!(res.deleted.len(), 3, "top.md, gone/c.md, gone/deep/d.md");
    assert_eq!(res.kept_unseen, 1, "locked/b.md survives a partial listing");
    let col = fx.collection;
    let ops = fx
        .cat
        .read(move |c| outbox::ops_after(c, col, 0, 100))
        .unwrap();
    let deletes = ops
        .ops
        .iter()
        .filter(|(_, o)| matches!(o, CoalescedOp::Delete { .. }))
        .count();
    assert_eq!(deletes, 3, "each sweep delete has an outbox op");
    let st = fx
        .cat
        .write_blocking(move |c| status::refresh(c, folder))
        .unwrap();
    assert_eq!(st.0, FolderStatus::Indexing, "files still pending");
}

#[test]
fn folder_status_synced_with_errors_and_removal_purges() {
    let fx = setup();
    let folder = add_folder(&fx.cat, fx.collection, &fx.root("st"));
    let good = add_file(&fx.cat, folder, "good.md", 1);
    let bad = add_file(&fx.cat, folder, "bad.pdf", 2);
    let o = t1(&fx.cat, good, "hello");
    fx.cat
        .write_blocking(move |c| {
            let scan = files::begin_scan(c, folder, NOW)?;
            files::mark_dir(c, scan, folder, "", true, None)?;
            files::observe(c, folder, scan, &obs("good.md", 1), NOW)?;
            files::observe(c, folder, scan, &obs("bad.pdf", 2), NOW)?;
            files::finish_scan(c, folder, scan, NOW)?;
            chunks::finalize_file(c, good, o.file_version, NOW)?;
            files::mark_dead(c, bad, ErrorCode::FileUnreadable, NOW)?;
            Ok(())
        })
        .unwrap();
    let (s, changed) = fx
        .cat
        .write_blocking(move |c| status::refresh(c, folder))
        .unwrap();
    assert_eq!(s, FolderStatus::SyncedWithErrors);
    assert!(changed);
    let dead = fx.cat.read(move |c| files::dead_letter(c, folder)).unwrap();
    assert_eq!(dead.len(), 1);
    assert_eq!(dead[0].error_code.as_deref(), Some("FileUnreadable"));
    // Removing the folder: purge op + cascade (no orphan chunk text).
    fx.cat
        .write_blocking(move |c| folders::remove_folder(c, folder, NOW))
        .unwrap();
    let (files_n, chunks_n, text_n): (i64, i64, i64) = fx
        .cat
        .read(|c| {
            c.query_row(
                "SELECT (SELECT count(*) FROM files), (SELECT count(*) FROM chunks),
                        (SELECT count(*) FROM chunk_text)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map_err(sr_catalog::map_db_err)
        })
        .unwrap();
    assert_eq!((files_n, chunks_n, text_n), (0, 0, 0));
    let col = fx.collection;
    let ops = fx
        .cat
        .read(move |c| outbox::ops_after(c, col, 0, 100))
        .unwrap();
    assert!(
        ops.ops
            .iter()
            .any(|(_, o)| *o == CoalescedOp::PurgeFolder { folder_id: folder })
    );
}

#[test]
fn overlapping_folder_in_same_collection_is_refused_with_plain_message() {
    let fx = setup();
    let root = fx.root("Projects");
    add_folder(&fx.cat, fx.collection, &root);
    let f = folders::NewFolder {
        collection_id: fx.collection,
        kind: sr_catalog::FolderKind::Indexed,
        root_path: root.join("sub"),
        volume_serial: None,
        is_primary: false,
        settings: Default::default(),
    };
    let e = fx
        .cat
        .write_blocking(move |c| folders::insert_folder(c, &f, NOW))
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::FolderOverlap);
    assert_eq!(
        e.user_message().body,
        "This folder is already part of \"Projects\"."
    );
}

#[test]
fn folder_settings_round_trip_and_bump_generation() {
    let fx = setup();
    let folder = add_folder(&fx.cat, fx.collection, &fx.root("cfg"));
    let mut s = fx
        .cat
        .read(move |c| folders::folder(c, folder))
        .unwrap()
        .settings;
    assert_eq!(s, Default::default());
    s.display_name = "Notes".into();
    s.include_extensions = vec!["md".into(), "txt".into()];
    s.file_types_preset = sr_common::config::FileTypesPreset::Custom;
    s.chunk_size_override = Some(256);
    let s2 = s.clone();
    let affects = fx
        .cat
        .write_blocking(move |c| folders::update_settings(c, folder, &s2, NOW))
        .unwrap();
    assert!(affects);
    let rec = fx.cat.read(move |c| folders::folder(c, folder)).unwrap();
    assert_eq!(rec.settings, s);
    assert_eq!(rec.generation, 1);
    assert_eq!(rec.display_name(), "Notes");
}

// --------------------------------------------------------------------------------- profiles

#[test]
fn shadow_index_swap_is_atomic_and_dims_are_enforced() {
    let fx = setup();
    let col = fx.collection;
    let (old, _) = fx
        .cat
        .read(move |c| profiles::active_index(c, col))
        .unwrap()
        .unwrap();
    assert_eq!(old.dims, 384);
    let std_spec = ProfileSpec::local(&STANDARD, &STANDARD.gpu_file);
    assert_eq!(std_spec.dims, 768);
    let (new_id, retired) = fx
        .cat
        .write_blocking(move |c| {
            let p = profiles::ensure_profile(c, &std_spec, NOW)?;
            let again = profiles::ensure_profile(c, &std_spec, NOW)?;
            assert_eq!(p, again, "fingerprint dedupes profiles");
            let shadow = profiles::create_vector_index(c, col, p, IndexState::Building, NOW)?;
            let retired = profiles::activate_index(c, col, shadow, NOW + 1)?;
            Ok((shadow, retired))
        })
        .unwrap();
    assert_eq!(retired, Some(old.id));
    let (act, prof) = fx
        .cat
        .read(move |c| profiles::active_index(c, col))
        .unwrap()
        .unwrap();
    assert_eq!(act.id, new_id);
    assert_eq!((act.dims, prof.spec.dims), (768, 768));
    assert_eq!(prof.spec.tier, sr_catalog::model::ProfileTier::Standard);
    // A query vector from another embedding space is refused, never compared.
    assert!(profiles::check_query_dims(&act, 768).is_ok());
    let e = profiles::check_query_dims(&act, 384).unwrap_err();
    assert_eq!(e.code, ErrorCode::Internal);
    // The DB itself rejects an index whose dims differ from its profile.
    let e = fx
        .cat
        .write_blocking(move |c| {
            c.execute(
                "INSERT INTO vector_indexes(id, collection_id, profile_id, dims, lance_table, state, created_at)
                 VALUES (randomblob(16), ?1, 1, 1024, 'bad', 'building', 0)",
                [col.to_bytes()],
            )
            .map_err(sr_catalog::map_db_err)
        })
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::Conflict);
    // Only one active index per collection.
    let e = fx
        .cat
        .write_blocking(move |c| {
            c.execute(
                "UPDATE vector_indexes SET state = 'active' WHERE state = 'retired'",
                [],
            )
            .map_err(sr_catalog::map_db_err)
        })
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::Conflict);
}

#[test]
fn meta_records_tier_detection() {
    // Detected facts only: the user's tier override lives in settings.toml (Amendment 4.10).
    let fx = setup();
    use sr_common::tier::{HwInfo, Thresholds, select_tier};
    let hw = HwInfo {
        total_ram: 16 << 30,
        physical_cores: 8,
        logical_cores: 16,
        avx2: true,
        avx512_vnni: false,
        free_disk: 100 << 30,
        gpus: vec![],
    };
    let d = meta::HwDetection {
        decision: select_tier(&hw, &Thresholds::default()),
        hw,
        detected_at: NOW,
    };
    let d2 = d.clone();
    fx.cat
        .write_blocking(move |c| meta::set_hw_detection(c, &d2))
        .unwrap();
    let (got, id1, id2) = fx
        .cat
        .write_blocking(|c| {
            Ok((
                meta::hw_detection(c)?,
                meta::install_id(c)?,
                meta::install_id(c)?,
            ))
        })
        .unwrap();
    assert_eq!(got, Some(d));
    assert_eq!(id1, id2);
    let no_override: Option<String> = fx.cat.read(|c| meta::get(c, "tier_override")).unwrap();
    assert_eq!(no_override, None);
}

// --------------------------------------------------------------------------------- read pool

#[test]
fn read_pool_serves_snapshots_while_writer_holds_a_long_transaction() {
    let fx = setup();
    let cat = Arc::new(fx.cat);
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let w = {
        let cat = Arc::clone(&cat);
        std::thread::spawn(move || {
            cat.write_blocking(move |c| {
                let tx = c
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(sr_catalog::map_db_err)?;
                tx.execute("INSERT INTO meta(key, value) VALUES ('long', 'tx')", [])
                    .map_err(sr_catalog::map_db_err)?;
                let _ = started_tx.send(());
                std::thread::sleep(Duration::from_millis(800));
                tx.commit().map_err(sr_catalog::map_db_err)
            })
        })
    };
    started_rx.recv().unwrap();
    let t0 = Instant::now();
    let readers: Vec<_> = (0..6)
        .map(|_| {
            let cat = Arc::clone(&cat);
            std::thread::spawn(move || cat.read(|c| meta::get(c, "long")).unwrap())
        })
        .collect();
    for r in readers {
        assert_eq!(r.join().unwrap(), None, "uncommitted data is invisible");
    }
    assert!(
        t0.elapsed() < Duration::from_millis(500),
        "reads were blocked by the writer: {:?}",
        t0.elapsed()
    );
    w.join().unwrap().unwrap();
    assert_eq!(
        cat.read(|c| meta::get(c, "long")).unwrap().as_deref(),
        Some("tx")
    );
}

#[test]
fn writer_survives_a_panicking_job_and_rolls_it_back() {
    let fx = setup();
    let r = fx.cat.write_blocking(|c| -> sr_common::Result<()> {
        c.execute("INSERT INTO meta(key, value) VALUES ('p', '1')", [])
            .map_err(sr_catalog::map_db_err)?;
        let tx = c.unchecked_transaction().map_err(sr_catalog::map_db_err)?;
        tx.execute("INSERT INTO meta(key, value) VALUES ('q', '1')", [])
            .map_err(sr_catalog::map_db_err)?;
        panic!("boom");
    });
    assert_eq!(r.unwrap_err().code, ErrorCode::Internal);
    assert_eq!(
        fx.cat.read(|c| meta::get(c, "p")).unwrap().as_deref(),
        Some("1")
    );
    assert_eq!(fx.cat.read(|c| meta::get(c, "q")).unwrap(), None);
    // Still serving.
    fx.cat
        .write_blocking(|c| meta::set(c, "after", "ok"))
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn async_write_and_read() {
    let fx = setup();
    fx.cat.write(|c| meta::set(c, "k", "v")).await.unwrap();
    let v = fx.cat.read_async(|c| meta::get(c, "k")).await.unwrap();
    assert_eq!(v.as_deref(), Some("v"));
    drop(Catalog::open(&fx.dir.path().join("second.db")).unwrap());
}
