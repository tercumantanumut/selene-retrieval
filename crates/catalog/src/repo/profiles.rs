//! Embedding profiles, collections and vector indexes (Amendment 1; §2.4, §2.8).
//!
//! Every vector index records the profile (model, revision, variant sha256, dims) it was built
//! with. Queries are embedded with the **active index's** profile; a query vector whose length
//! differs from the index dims is refused ([`check_query_dims`]), never compared.

use rusqlite::{Connection, OptionalExtension, params};
use sr_common::error::{Error, ErrorCode, Result};
use sr_common::ids::{ChunkId, CollectionId, VectorIndexId};
use sr_common::tier::{ModelFile, TierProfile};

use crate::db::{DbResultExt, atomic};
use crate::model::{IndexState, ProfileId, ProfilePrecision, ProfileTier, RowExt, Seq, Store};
use crate::repo::outbox;

/// Name of the collection created on first run (Basic UI shows folders only).
pub const DEFAULT_COLLECTION_NAME: &str = "My folders";

/// Everything that identifies an embedding space.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileSpec {
    pub provider: String,
    pub model: String,
    pub model_revision: String,
    pub dims: u32,
    pub pooling: String,
    pub query_prefix: String,
    pub doc_prefix: String,
    pub variant_path: String,
    pub variant_sha256: String,
    pub precision: ProfilePrecision,
    pub tier: ProfileTier,
    pub is_remote: bool,
}

impl ProfileSpec {
    /// Local profile for a tier and the exact model file the index is built with (sticky variant).
    pub fn local(p: &TierProfile, file: &ModelFile) -> Self {
        use sr_common::tier::{Precision, Tier};
        Self {
            provider: "local".into(),
            model: p.model_id.into(),
            model_revision: file.hf_revision.into(),
            dims: p.dims,
            pooling: p.pooling.as_str().into(),
            query_prefix: p.query_prefix.into(),
            doc_prefix: p.doc_prefix.into(),
            variant_path: format!("{}/{}", file.hf_repo, file.path),
            variant_sha256: file.sha256.into(),
            precision: match file.precision {
                Precision::Fp32 => ProfilePrecision::Fp32,
                Precision::Fp16 => ProfilePrecision::Fp16,
                Precision::Int8 => ProfilePrecision::Int8,
            },
            tier: match p.tier {
                Tier::Minimum => ProfileTier::Minimum,
                Tier::Standard => ProfileTier::Standard,
                Tier::High => ProfileTier::High,
            },
            is_remote: false,
        }
    }

    /// `blake3(provider‖model‖revision‖variant_sha256‖dims‖pooling‖prefixes)`, length-prefixed.
    pub fn fingerprint(&self) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        h.update(b"sr.profile_fp.v1");
        let dims = self.dims.to_le_bytes();
        for f in [
            self.provider.as_bytes(),
            self.model.as_bytes(),
            self.model_revision.as_bytes(),
            self.variant_sha256.as_bytes(),
            &dims[..],
            self.pooling.as_bytes(),
            self.query_prefix.as_bytes(),
            self.doc_prefix.as_bytes(),
        ] {
            h.update(&(f.len() as u64).to_le_bytes());
            h.update(f);
        }
        *h.finalize().as_bytes()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileRecord {
    pub id: ProfileId,
    pub spec: ProfileSpec,
    pub fingerprint: [u8; 32],
}

/// Insert the profile if its fingerprint is new; returns its id either way.
pub fn ensure_profile(conn: &Connection, spec: &ProfileSpec, now: i64) -> Result<ProfileId> {
    let fp = spec.fingerprint();
    conn.prepare_cached(
        "INSERT INTO embedding_profiles(provider, model, model_revision, dims, pooling, query_prefix,
             doc_prefix, variant_path, variant_sha256, precision, tier, fingerprint, is_remote, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
         ON CONFLICT(fingerprint) DO NOTHING",
    )
    .db()?
    .execute(params![
        spec.provider,
        spec.model,
        spec.model_revision,
        spec.dims,
        spec.pooling,
        spec.query_prefix,
        spec.doc_prefix,
        spec.variant_path,
        spec.variant_sha256,
        spec.precision,
        spec.tier,
        fp,
        spec.is_remote,
        now
    ])
    .db()?;
    conn.query_row(
        "SELECT id FROM embedding_profiles WHERE fingerprint = ?1",
        [fp],
        |r| r.get(0),
    )
    .db()
}

const PROFILE_COLS: &str =
    "id, provider, model, model_revision, dims, pooling, query_prefix, doc_prefix,
     variant_path, variant_sha256, precision, tier, is_remote, fingerprint";

fn profile_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ProfileRecord> {
    Ok(ProfileRecord {
        id: r.get(0)?,
        spec: ProfileSpec {
            provider: r.get(1)?,
            model: r.get(2)?,
            model_revision: r.get(3)?,
            dims: r.get(4)?,
            pooling: r.get(5)?,
            query_prefix: r.get(6)?,
            doc_prefix: r.get(7)?,
            variant_path: r.get(8)?,
            variant_sha256: r.get(9)?,
            precision: r.get(10)?,
            tier: r.get(11)?,
            is_remote: r.get(12)?,
        },
        fingerprint: r.get(13)?,
    })
}

pub fn profile(conn: &Connection, id: ProfileId) -> Result<ProfileRecord> {
    conn.query_row(
        &format!("SELECT {PROFILE_COLS} FROM embedding_profiles WHERE id = ?1"),
        [id],
        profile_from_row,
    )
    .db()
}

// ---------------------------------------------------------------------------------- collections

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectionRecord {
    pub id: CollectionId,
    pub name: String,
    pub embedding_profile_id: ProfileId,
    pub active_index_id: Option<VectorIndexId>,
    pub is_default: bool,
}

pub fn create_collection(
    conn: &Connection,
    name: &str,
    profile_id: ProfileId,
    is_default: bool,
    now: i64,
) -> Result<CollectionId> {
    let id = CollectionId::new();
    conn.execute(
        "INSERT INTO collections(id, name, embedding_profile_id, is_default, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id.to_bytes(), name, profile_id, is_default, now],
    )
    .db()?;
    Ok(id)
}

pub fn collection(conn: &Connection, id: CollectionId) -> Result<CollectionRecord> {
    conn.query_row(
        "SELECT id, name, embedding_profile_id, active_index_id, is_default FROM collections WHERE id = ?1",
        [id.to_bytes()],
        |r| {
            Ok(CollectionRecord {
                id: r.collection_id(0)?,
                name: r.get(1)?,
                embedding_profile_id: r.get(2)?,
                active_index_id: r.opt_index_id(3)?,
                is_default: r.get(4)?,
            })
        },
    )
    .db()
}

pub fn collections(conn: &Connection) -> Result<Vec<CollectionRecord>> {
    let mut st = conn
        .prepare("SELECT id FROM collections ORDER BY created_at, id")
        .db()?;
    let ids = st
        .query_map([], |r| r.collection_id(0))
        .db()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .db()?;
    ids.into_iter().map(|id| collection(conn, id)).collect()
}

/// The default collection, created on first run together with its active vector index.
pub fn ensure_default_collection(
    conn: &Connection,
    profile_id: ProfileId,
    now: i64,
) -> Result<CollectionId> {
    atomic(conn, |c| {
        let existing: Option<[u8; 16]> = c
            .query_row("SELECT id FROM collections WHERE is_default = 1", [], |r| {
                r.get(0)
            })
            .optional()
            .db()?;
        if let Some(b) = existing {
            return Ok(CollectionId::from_bytes(b));
        }
        let id = create_collection(c, DEFAULT_COLLECTION_NAME, profile_id, true, now)?;
        create_vector_index(c, id, profile_id, IndexState::Active, now)?;
        Ok(id)
    })
}

// ------------------------------------------------------------------------------ vector indexes

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorIndexRecord {
    pub id: VectorIndexId,
    pub collection_id: CollectionId,
    pub profile_id: ProfileId,
    pub dims: u32,
    pub lance_table: String,
    pub state: IndexState,
    pub rows_total: u64,
    pub rows_done: u64,
    pub checkpoint_after_chunk_id: Option<ChunkId>,
    pub start_seq: Seq,
    pub failure_code: Option<String>,
}

const INDEX_COLS: &str =
    "id, collection_id, profile_id, dims, lance_table, state, rows_total, rows_done,
     checkpoint_after_chunk_id, start_seq, failure_code";

fn index_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<VectorIndexRecord> {
    Ok(VectorIndexRecord {
        id: r.index_id(0)?,
        collection_id: r.collection_id(1)?,
        profile_id: r.get(2)?,
        dims: r.get(3)?,
        lance_table: r.get(4)?,
        state: r.get(5)?,
        rows_total: r.get(6)?,
        rows_done: r.get(7)?,
        checkpoint_after_chunk_id: r.get::<_, Option<[u8; 32]>>(8)?.map(ChunkId::from_bytes),
        start_seq: r.get(9)?,
        failure_code: r.get(10)?,
    })
}

/// Creates an index row (Lance table `vec_<id>`). `Active` is only valid for a collection with no
/// active index yet (first run); a model change creates a `Building` shadow and later
/// [`activate_index`]es it. Dims come from the profile (a trigger re-checks them).
pub fn create_vector_index(
    conn: &Connection,
    collection_id: CollectionId,
    profile_id: ProfileId,
    state: IndexState,
    now: i64,
) -> Result<VectorIndexId> {
    if !matches!(state, IndexState::Active | IndexState::Building) {
        return Err(
            Error::new(ErrorCode::InvalidInput).with_detail("new index must be active or building")
        );
    }
    atomic(conn, |c| {
        let p = profile(c, profile_id)?;
        let id = VectorIndexId::new();
        let start_seq = outbox::max_seq(c)?;
        c.execute(
            "INSERT INTO vector_indexes(id, collection_id, profile_id, dims, lance_table, state, start_seq,
                 created_at, activated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, CASE WHEN ?6 = 'active' THEN ?8 END)",
            params![
                id.to_bytes(),
                collection_id.to_bytes(),
                profile_id,
                p.spec.dims,
                format!("vec_{}", id.to_string().to_ascii_lowercase()),
                state,
                start_seq,
                now
            ],
        )
        .db()?;
        // Replay into a new index starts at its creation point.
        outbox::set_watermark(c, Store::Lance, &id.to_bytes(), start_seq)?;
        if state == IndexState::Active {
            c.execute(
                "UPDATE collections SET active_index_id = ?1, embedding_profile_id = ?2 WHERE id = ?3",
                params![id.to_bytes(), profile_id, collection_id.to_bytes()],
            )
            .db()?;
        }
        Ok(id)
    })
}

pub fn vector_index(conn: &Connection, id: VectorIndexId) -> Result<VectorIndexRecord> {
    conn.query_row(
        &format!("SELECT {INDEX_COLS} FROM vector_indexes WHERE id = ?1"),
        [id.to_bytes()],
        index_from_row,
    )
    .db()
}

/// The index queries must use, with its profile.
pub fn active_index(
    conn: &Connection,
    collection_id: CollectionId,
) -> Result<Option<(VectorIndexRecord, ProfileRecord)>> {
    let idx = conn
        .query_row(
            &format!(
                "SELECT {INDEX_COLS} FROM vector_indexes WHERE collection_id = ?1 AND state = 'active'"
            ),
            [collection_id.to_bytes()],
            index_from_row,
        )
        .optional()
        .db()?;
    match idx {
        Some(i) => {
            let p = profile(conn, i.profile_id)?;
            Ok(Some((i, p)))
        }
        None => Ok(None),
    }
}

/// Live (active or building) indexes of a collection; every one of them receives outbox ops.
pub fn live_indexes(
    conn: &Connection,
    collection_id: CollectionId,
) -> Result<Vec<VectorIndexRecord>> {
    let mut st = conn
        .prepare_cached(&format!(
            "SELECT {INDEX_COLS} FROM vector_indexes
             WHERE collection_id = ?1 AND state IN ('active','building') ORDER BY created_at"
        ))
        .db()?;
    st.query_map([collection_id.to_bytes()], index_from_row)
        .db()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .db()
}

/// Shadow-build checkpoint: data first, checkpoint second (§2.8).
pub fn update_index_progress(
    conn: &Connection,
    id: VectorIndexId,
    rows_done: u64,
    rows_total: u64,
    checkpoint: Option<ChunkId>,
) -> Result<()> {
    conn.execute(
        "UPDATE vector_indexes SET rows_done = ?2, rows_total = ?3,
             checkpoint_after_chunk_id = coalesce(?4, checkpoint_after_chunk_id)
         WHERE id = ?1 AND state = 'building'",
        params![
            id.to_bytes(),
            rows_done,
            rows_total,
            checkpoint.map(|c| *c.as_bytes())
        ],
    )
    .db()?;
    Ok(())
}

/// Atomic swap (one transaction): old active → retired, `new` → active, pointer updated.
/// Returns the retired index id.
pub fn activate_index(
    conn: &Connection,
    collection_id: CollectionId,
    new_id: VectorIndexId,
    now: i64,
) -> Result<Option<VectorIndexId>> {
    atomic(conn, |c| {
        let new = vector_index(c, new_id)?;
        if new.collection_id != collection_id || new.state != IndexState::Building {
            return Err(Error::new(ErrorCode::Conflict)
                .with_detail("index is not a building shadow of this collection"));
        }
        let old = active_index(c, collection_id)?.map(|(i, _)| i.id);
        c.execute(
            "UPDATE vector_indexes SET state = 'retired', retired_at = ?2
             WHERE collection_id = ?1 AND state = 'active'",
            params![collection_id.to_bytes(), now],
        )
        .db()?;
        c.execute(
            "UPDATE vector_indexes SET state = 'active', activated_at = ?2 WHERE id = ?1",
            params![new_id.to_bytes(), now],
        )
        .db()?;
        c.execute(
            "UPDATE collections SET active_index_id = ?1, embedding_profile_id = ?2 WHERE id = ?3",
            params![new_id.to_bytes(), new.profile_id, collection_id.to_bytes()],
        )
        .db()?;
        Ok(old)
    })
}

/// Marks a shadow build failed (e.g. `ModelIncompatible`); the active index is untouched.
pub fn fail_index(conn: &Connection, id: VectorIndexId, code: ErrorCode) -> Result<()> {
    conn.execute(
        "UPDATE vector_indexes SET state = 'failed', failure_code = ?2 WHERE id = ?1 AND state = 'building'",
        params![id.to_bytes(), code.as_str()],
    )
    .db()?;
    Ok(())
}

/// Refuses a query vector built for another embedding space.
pub fn check_query_dims(index: &VectorIndexRecord, query_len: usize) -> Result<()> {
    if usize::try_from(index.dims).ok() == Some(query_len) {
        Ok(())
    } else {
        tracing::error!(index = %index.id, dims = index.dims, query_len, "query vector dims mismatch");
        Err(Error::new(ErrorCode::Internal).with_detail(format!(
            "query vector has {query_len} dims, index {} has {}",
            index.id, index.dims
        )))
    }
}
