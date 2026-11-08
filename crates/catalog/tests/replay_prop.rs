//! Property test (D3, M1 exit "kill -9 at random points resumes with no duplicates or orphans"):
//! random interleavings of T1 writes, deletes, store applies and crashes at every point between
//! T1 and T2 always leave (a) replayable ops in the outbox and (b) after restart + replay, a store
//! whose chunk set equals the catalog's, consistent refcounts and no file stuck pending.
// Test crate: helpers outside #[test] fns may unwrap (clippy.toml covers only #[test] fns).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::{HashMap, HashSet};

use common::*;
use proptest::prelude::*;
use sr_catalog::repo::outbox::{self, CoalescedOp};
use sr_catalog::repo::{chunks, files, profiles};
use sr_catalog::{Catalog, Seq, Store};
use sr_common::ids::{ChunkId, CollectionId, FileId, FolderId, VectorIndexId};

#[derive(Debug, Clone)]
enum Op {
    /// T1 for file slot `slot` with content variant `v` (a deleted slot is re-created first).
    Write { slot: usize, v: u8 },
    /// Watcher delete.
    Delete { slot: usize },
    /// Store applies the outbox; `persist` = watermark persisted; `t2` = T2 ran.
    Apply { persist: bool, t2: bool },
    /// kill -9 + restart (all committed SQLite state survives; nothing in flight does).
    Crash,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        5 => (0..4usize, 0..4u8).prop_map(|(slot, v)| Op::Write { slot, v }),
        1 => (0..4usize).prop_map(|slot| Op::Delete { slot }),
        3 => (any::<bool>(), any::<bool>()).prop_map(|(persist, t2)| Op::Apply { persist, t2 }),
        2 => Just(Op::Crash),
    ]
}

/// A derived store (stands in for Lance + tantivy): rows keyed by chunk id, plus its own durable
/// watermark (the commit payload).
#[derive(Default)]
struct FakeStore {
    rows: HashMap<ChunkId, FileId>,
    folder_of: HashMap<FileId, FolderId>,
    watermark: Seq,
}

impl FakeStore {
    fn apply(&mut self, cat: &Catalog, col: CollectionId) -> Seq {
        let after = self.watermark;
        let batch = cat
            .read(move |c| outbox::ops_after(c, col, after, 10_000))
            .unwrap();
        for (_, op) in &batch.ops {
            match op {
                CoalescedOp::Upsert { file_id, .. } => {
                    let f = *file_id;
                    self.rows.retain(|_, v| *v != f);
                    let cs = cat.read(move |c| chunks::file_chunks(c, f)).unwrap();
                    let folder = cat.read(move |c| files::file(c, f)).unwrap().folder_id;
                    self.folder_of.insert(f, folder);
                    for ch in cs {
                        assert!(self.rows.insert(ch.chunk_id, f).is_none_or(|o| o == f));
                    }
                }
                CoalescedOp::Delete { file_id } => {
                    let f = *file_id;
                    self.rows.retain(|_, v| *v != f);
                }
                CoalescedOp::PurgeFolder { folder_id } => {
                    let fo = *folder_id;
                    let gone: HashSet<FileId> = self
                        .folder_of
                        .iter()
                        .filter(|(_, v)| **v == fo)
                        .map(|(k, _)| *k)
                        .collect();
                    self.rows.retain(|_, v| !gone.contains(v));
                }
            }
        }
        batch.through_seq
    }

    fn persist(&mut self, cat: &Catalog, col: CollectionId, idx: VectorIndexId, seq: Seq) {
        self.watermark = seq;
        cat.write_blocking(move |c| {
            outbox::set_watermark(c, Store::Tantivy, &col.to_bytes(), seq)?;
            outbox::set_watermark(c, Store::Lance, &idx.to_bytes(), seq)
        })
        .unwrap();
    }
}

fn content(slot: usize, v: u8) -> String {
    // Some lines repeat across slots/variants so chunk_text is shared (refcount > 1).
    format!(
        "shared header\nslot {slot}\nvariant {v}\nshared footer {}",
        v % 2
    )
}

fn run(ops: &[Op]) {
    let mut fx = setup();
    let col = fx.collection;
    let folder = add_folder(&fx.cat, col, &fx.root("p"));
    let idx = fx
        .cat
        .read(move |c| profiles::active_index(c, col))
        .unwrap()
        .unwrap()
        .0
        .id;
    let mut slots: Vec<Option<FileId>> = vec![None; 4];
    let mut ntfs = 0u8;
    let mut store = FakeStore::default();

    for op in ops {
        match op {
            Op::Write { slot, v } => {
                let f = match slots[*slot] {
                    Some(f) => f,
                    None => {
                        ntfs = ntfs.wrapping_add(1);
                        let f = add_file(&fx.cat, folder, &format!("f{slot}-{ntfs}.md"), ntfs);
                        slots[*slot] = Some(f);
                        f
                    }
                };
                let out = t1(&fx.cat, f, &content(*slot, *v));
                // (a) every committed T1 is replayable until its T2 has pruned it.
                let s = out.seq;
                let pending = fx
                    .cat
                    .read(move |c| outbox::ops_after(c, col, s - 1, 1))
                    .unwrap();
                assert_eq!(pending.raw, 1);
            }
            Op::Delete { slot } => {
                if let Some(f) = slots[*slot].take() {
                    fx.cat
                        .write_blocking(move |c| files::delete_file(c, f, NOW))
                        .unwrap();
                }
            }
            Op::Apply { persist, t2 } => {
                let through = store.apply(&fx.cat, col);
                if *persist {
                    store.persist(&fx.cat, col, idx, through);
                    if *t2 {
                        fx.cat
                            .write_blocking(move |c| chunks::finalize_applied(c, col, NOW))
                            .unwrap();
                    }
                }
            }
            Op::Crash => fx.reopen(),
        }
    }

    // Final crash, restart, replay from the store's own watermark, startup T2.
    fx.reopen();
    let wm = store.watermark;
    let cat_wm = fx
        .cat
        .read(move |c| outbox::watermark(c, Store::Tantivy, &col.to_bytes()))
        .unwrap();
    assert_eq!(cat_wm, wm, "the cached watermark matches the store's");
    let through = store.apply(&fx.cat, col);
    store.persist(&fx.cat, col, idx, through);
    fx.cat
        .write_blocking(move |c| chunks::finalize_applied(c, col, NOW))
        .unwrap();

    // (b) store == catalog: no duplicates, no orphans, nothing missing.
    let catalog_rows: HashMap<ChunkId, FileId> = fx
        .cat
        .read(|c| {
            let mut st = c
                .prepare("SELECT chunk_id, file_id FROM chunks")
                .map_err(sr_catalog::map_db_err)?;
            let v = st
                .query_map([], |r| {
                    Ok((
                        ChunkId::from_bytes(r.get(0)?),
                        FileId::from_bytes(r.get(1)?),
                    ))
                })
                .map_err(sr_catalog::map_db_err)?
                .collect::<rusqlite::Result<HashMap<_, _>>>()
                .map_err(sr_catalog::map_db_err)?;
            Ok(v)
        })
        .unwrap();
    assert_eq!(store.rows, catalog_rows);

    // Catalog integrity: refcounts exact, no unreferenced text, no chunk without its file/text,
    // nothing stuck pending, outbox fully pruned.
    let (bad_ref, orphan_text, orphan_chunk, missing_text, stuck, ops_left): (i64, i64, i64, i64, i64, i64) = fx
        .cat
        .read(|c| {
            c.query_row(
                "SELECT
                   (SELECT count(*) FROM chunk_text t
                      WHERE t.refcount <> (SELECT count(*) FROM chunks c WHERE c.text_hash = t.text_hash)),
                   (SELECT count(*) FROM chunk_text WHERE refcount <= 0),
                   (SELECT count(*) FROM chunks c WHERE NOT EXISTS (SELECT 1 FROM files f WHERE f.id = c.file_id)),
                   (SELECT count(*) FROM chunks c WHERE NOT EXISTS
                      (SELECT 1 FROM chunk_text t WHERE t.text_hash = c.text_hash)),
                   (SELECT count(*) FROM files WHERE state = 'pending_index' AND version > 0),
                   (SELECT count(*) FROM index_ops)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .map_err(sr_catalog::map_db_err)
        })
        .unwrap();
    assert_eq!(
        (
            bad_ref,
            orphan_text,
            orphan_chunk,
            missing_text,
            stuck,
            ops_left
        ),
        (0, 0, 0, 0, 0, 0)
    );
    // Every live slot's chunks are its current version's.
    for f in slots.into_iter().flatten() {
        let rec = fx.cat.read(move |c| files::file(c, f)).unwrap();
        let cs = fx.cat.read(move |c| chunks::file_chunks(c, f)).unwrap();
        assert!(cs.iter().all(|c| c.file_version == rec.version));
    }
    let fk: i64 = fx
        .cat
        .read(|c| {
            c.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .map_err(sr_catalog::map_db_err)
        })
        .unwrap();
    assert_eq!(fk, 0);
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    #[test]
    fn crash_between_t1_and_t2_leaves_replayable_ops_and_no_orphans(
        ops in prop::collection::vec(op(), 1..40)
    ) {
        run(&ops);
    }
}

/// The exact gap the property test found: watermark persisted, crash before T2.
#[test]
fn crash_after_watermark_before_t2_is_recovered_at_startup() {
    run(&[
        Op::Write { slot: 0, v: 1 },
        Op::Apply {
            persist: true,
            t2: false,
        },
        Op::Crash,
    ]);
}
