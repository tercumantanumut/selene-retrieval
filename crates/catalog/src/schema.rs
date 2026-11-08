//! Versioned migrations (`rusqlite_migration`, tracked in `PRAGMA user_version`).
//!
//! Rules: migrations are append-only; never edit a shipped one. Every table is `STRICT`.
//! Ids: ULIDs are stored as 16-byte BLOBs, blake3 ids as 32-byte BLOBs, times as Unix ms.
//! Schema source: docs/02-architecture.md §2.4 plus architect review D2–D4.

use rusqlite_migration::{M, Migrations};

/// Migration 1: the M1 catalog.
const V1: &str = r#"
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) STRICT;

CREATE TABLE embedding_profiles (
    id             INTEGER PRIMARY KEY,
    provider       TEXT    NOT NULL,
    model          TEXT    NOT NULL,
    model_revision TEXT    NOT NULL,
    dims           INTEGER NOT NULL CHECK (dims > 0),
    pooling        TEXT    NOT NULL CHECK (pooling IN ('cls','mean')),
    query_prefix   TEXT    NOT NULL DEFAULT '',
    doc_prefix     TEXT    NOT NULL DEFAULT '',
    variant_path   TEXT    NOT NULL,
    variant_sha256 TEXT    NOT NULL,
    precision      TEXT    NOT NULL CHECK (precision IN ('fp32','fp16','int8','remote')),
    tier           TEXT    NOT NULL CHECK (tier IN ('minimum','standard','high','custom')),
    fingerprint    BLOB    NOT NULL UNIQUE CHECK (length(fingerprint) = 32),
    is_remote      INTEGER NOT NULL DEFAULT 0 CHECK (is_remote IN (0,1)),
    state          TEXT    NOT NULL DEFAULT 'active' CHECK (state IN ('active','building','retired')),
    created_at     INTEGER NOT NULL
) STRICT;

CREATE TABLE collections (
    id                   BLOB    PRIMARY KEY CHECK (length(id) = 16),
    name                 TEXT    NOT NULL COLLATE NOCASE UNIQUE,
    embedding_profile_id INTEGER NOT NULL REFERENCES embedding_profiles(id),
    active_index_id      BLOB    NULL REFERENCES vector_indexes(id) DEFERRABLE INITIALLY DEFERRED,
    is_default           INTEGER NOT NULL DEFAULT 0 CHECK (is_default IN (0,1)),
    settings_json        TEXT    NOT NULL DEFAULT '{}',
    created_at           INTEGER NOT NULL
) STRICT;
CREATE UNIQUE INDEX ux_collections_default ON collections(is_default) WHERE is_default = 1;

CREATE TABLE vector_indexes (
    id                        BLOB    PRIMARY KEY CHECK (length(id) = 16),
    collection_id             BLOB    NOT NULL REFERENCES collections(id) ON DELETE CASCADE,
    profile_id                INTEGER NOT NULL REFERENCES embedding_profiles(id),
    dims                      INTEGER NOT NULL CHECK (dims > 0),
    lance_table               TEXT    NOT NULL UNIQUE,
    state                     TEXT    NOT NULL CHECK (state IN ('building','active','retired','failed')),
    rows_total                INTEGER NOT NULL DEFAULT 0,
    rows_done                 INTEGER NOT NULL DEFAULT 0,
    checkpoint_after_chunk_id BLOB    NULL CHECK (checkpoint_after_chunk_id IS NULL OR length(checkpoint_after_chunk_id) = 32),
    start_seq                 INTEGER NOT NULL DEFAULT 0,
    failure_code              TEXT    NULL,
    created_at                INTEGER NOT NULL,
    activated_at              INTEGER NULL,
    retired_at                INTEGER NULL
) STRICT;
CREATE UNIQUE INDEX ux_vector_indexes_active   ON vector_indexes(collection_id) WHERE state = 'active';
CREATE UNIQUE INDEX ux_vector_indexes_building ON vector_indexes(collection_id) WHERE state = 'building';
CREATE INDEX ix_vector_indexes_profile ON vector_indexes(profile_id);
-- An index's dims must equal its profile's dims (a mismatch is never stored).
CREATE TRIGGER tr_vector_indexes_dims BEFORE INSERT ON vector_indexes
WHEN NEW.dims <> (SELECT dims FROM embedding_profiles WHERE id = NEW.profile_id)
BEGIN SELECT RAISE(ABORT, 'vector index dims differ from its profile'); END;
-- The active pointer may only name an active index of the same collection.
CREATE TRIGGER tr_collections_active_index BEFORE UPDATE OF active_index_id ON collections
WHEN NEW.active_index_id IS NOT NULL AND NOT EXISTS (
    SELECT 1 FROM vector_indexes v
    WHERE v.id = NEW.active_index_id AND v.collection_id = NEW.id AND v.state = 'active')
BEGIN SELECT RAISE(ABORT, 'active_index_id must name an active index of this collection'); END;

CREATE TABLE model_files (
    sha256     TEXT    PRIMARY KEY,
    repo       TEXT    NOT NULL,
    revision   TEXT    NOT NULL,
    path       TEXT    NOT NULL,
    bytes      INTEGER NOT NULL,
    bytes_have INTEGER NOT NULL DEFAULT 0,
    state      TEXT    NOT NULL CHECK (state IN ('partial','verified','bundled')),
    updated_at INTEGER NOT NULL
) STRICT;

CREATE TABLE folders (
    id                       BLOB    PRIMARY KEY CHECK (length(id) = 16),
    collection_id            BLOB    NOT NULL REFERENCES collections(id) ON DELETE CASCADE,
    kind                     TEXT    NOT NULL CHECK (kind IN ('indexed','access_grant')),
    display_name             TEXT    NOT NULL DEFAULT '',
    is_primary               INTEGER NOT NULL DEFAULT 0 CHECK (is_primary IN (0,1)),
    root_path                TEXT    NOT NULL,
    root_key                 TEXT    NOT NULL,
    volume_serial            INTEGER NULL,
    recursive                INTEGER NOT NULL DEFAULT 1 CHECK (recursive IN (0,1)),
    indexing_mode            TEXT    NOT NULL DEFAULT 'vector' CHECK (indexing_mode IN ('files_only','vector')),
    keep_up_to_date          INTEGER NOT NULL DEFAULT 1 CHECK (keep_up_to_date IN (0,1)),
    sync_mode                TEXT    NOT NULL DEFAULT 'auto' CHECK (sync_mode IN ('auto','manual','scheduled','triggered')),
    cadence_min              INTEGER NOT NULL DEFAULT 60,
    reindex_policy           TEXT    NOT NULL DEFAULT 'smart' CHECK (reindex_policy IN ('smart','always','never')),
    file_types_preset        TEXT    NOT NULL DEFAULT 'documents_and_code'
                             CHECK (file_types_preset IN ('documents','code','documents_and_code','all_text','custom')),
    include_extensions       TEXT    NOT NULL DEFAULT '[]',
    exclude_globs            TEXT    NOT NULL DEFAULT '[]',
    file_type_filters        TEXT    NOT NULL DEFAULT '{}',
    max_file_bytes           INTEGER NOT NULL DEFAULT 10485760,
    chunk_preset             TEXT    NOT NULL DEFAULT 'balanced' CHECK (chunk_preset IN ('balanced','small','large','custom')),
    chunk_size_override      INTEGER NULL,
    chunk_overlap_override   INTEGER NULL,
    follow_symlinks          INTEGER NOT NULL DEFAULT 0 CHECK (follow_symlinks IN (0,1)),
    include_online_only      INTEGER NOT NULL DEFAULT 0 CHECK (include_online_only IN (0,1)),
    paused                   INTEGER NOT NULL DEFAULT 0 CHECK (paused IN (0,1)),
    available                INTEGER NOT NULL DEFAULT 1 CHECK (available IN (0,1)),
    status                   TEXT    NOT NULL DEFAULT 'pending',
    status_reason_code       TEXT    NULL,
    generation               INTEGER NOT NULL DEFAULT 0,
    last_full_scan_at        INTEGER NULL,
    last_scan_partial        INTEGER NOT NULL DEFAULT 0 CHECK (last_scan_partial IN (0,1)),
    last_success_at          INTEGER NULL,
    last_run_trigger         TEXT    NULL CHECK (last_run_trigger IS NULL OR last_run_trigger IN
                                 ('startup','watcher','schedule','manual','mcp','settings_change','reconnect')),
    stats_json               TEXT    NOT NULL DEFAULT '{}',
    last_run_metadata        TEXT    NULL,
    inherited_from_folder_id BLOB    NULL REFERENCES folders(id) ON DELETE SET NULL,
    provenance               TEXT    NULL,
    expires_at               INTEGER NULL,
    created_at               INTEGER NOT NULL,
    updated_at               INTEGER NOT NULL,
    UNIQUE (collection_id, root_key)
) STRICT;
CREATE INDEX ix_folders_inherited ON folders(inherited_from_folder_id) WHERE inherited_from_folder_id IS NOT NULL;

CREATE TABLE scans (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    folder_id   BLOB    NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
    started_at  INTEGER NOT NULL,
    finished_at INTEGER NULL,
    partial     INTEGER NULL CHECK (partial IS NULL OR partial IN (0,1))
) STRICT;
CREATE INDEX ix_scans_folder ON scans(folder_id, id);

CREATE TABLE scan_dirs (
    scan_id     INTEGER NOT NULL REFERENCES scans(id) ON DELETE CASCADE,
    folder_id   BLOB    NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
    rel_dir_key TEXT    NOT NULL,
    complete    INTEGER NOT NULL DEFAULT 0 CHECK (complete IN (0,1)),
    error_code  TEXT    NULL,
    PRIMARY KEY (scan_id, folder_id, rel_dir_key)
) STRICT, WITHOUT ROWID;
CREATE INDEX ix_scan_dirs_folder ON scan_dirs(folder_id);

-- `row` is an explicit INTEGER PRIMARY KEY so files_fts' content_rowid survives VACUUM.
CREATE TABLE files (
    row               INTEGER PRIMARY KEY,
    id                BLOB    NOT NULL UNIQUE CHECK (length(id) = 16),
    folder_id         BLOB    NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
    rel_path          TEXT    NOT NULL,
    path_key          TEXT    NOT NULL,
    dir_key           TEXT    NOT NULL,
    ext               TEXT    NOT NULL DEFAULT '',
    volume_serial     INTEGER NULL,
    ntfs_file_id      BLOB    NULL CHECK (ntfs_file_id IS NULL OR length(ntfs_file_id) = 16),
    size              INTEGER NOT NULL,
    mtime_ns          INTEGER NOT NULL,
    content_hash      BLOB    NULL CHECK (content_hash IS NULL OR length(content_hash) = 32),
    version           INTEGER NOT NULL DEFAULT 0,
    extractor_version TEXT    NULL,
    chunk_cfg_fp      BLOB    NULL CHECK (chunk_cfg_fp IS NULL OR length(chunk_cfg_fp) = 32),
    scan_id           INTEGER NOT NULL DEFAULT 0,
    state             TEXT    NOT NULL DEFAULT 'pending_index'
                      CHECK (state IN ('pending_index','indexed','skipped','dead')),
    skip_reason       TEXT    NULL,
    error_code        TEXT    NULL,
    attempts          INTEGER NOT NULL DEFAULT 0,
    created_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL,
    UNIQUE (folder_id, path_key)
) STRICT;
CREATE INDEX ix_files_ntfs   ON files(volume_serial, ntfs_file_id) WHERE ntfs_file_id IS NOT NULL;
CREATE INDEX ix_files_scan   ON files(folder_id, scan_id);
CREATE INDEX ix_files_state  ON files(folder_id, state);

CREATE VIRTUAL TABLE files_fts USING fts5(
    rel_path, content = 'files', content_rowid = 'row', tokenize = 'unicode61 remove_diacritics 2'
);
CREATE TRIGGER tr_files_fts_ai AFTER INSERT ON files BEGIN
    INSERT INTO files_fts(rowid, rel_path) VALUES (NEW.row, NEW.rel_path);
END;
CREATE TRIGGER tr_files_fts_ad AFTER DELETE ON files BEGIN
    INSERT INTO files_fts(files_fts, rowid, rel_path) VALUES ('delete', OLD.row, OLD.rel_path);
END;
CREATE TRIGGER tr_files_fts_au AFTER UPDATE OF rel_path ON files BEGIN
    INSERT INTO files_fts(files_fts, rowid, rel_path) VALUES ('delete', OLD.row, OLD.rel_path);
    INSERT INTO files_fts(rowid, rel_path) VALUES (NEW.row, NEW.rel_path);
END;

CREATE TABLE chunk_text (
    text_hash BLOB    PRIMARY KEY CHECK (length(text_hash) = 32),
    zstd      BLOB    NOT NULL,
    bytes     INTEGER NOT NULL,
    refcount  INTEGER NOT NULL DEFAULT 0 CHECK (refcount >= 0)
) STRICT, WITHOUT ROWID;

CREATE TABLE chunks (
    chunk_id     BLOB    PRIMARY KEY CHECK (length(chunk_id) = 32),
    file_id      BLOB    NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    file_version INTEGER NOT NULL,
    ord          INTEGER NOT NULL,
    byte_start   INTEGER NOT NULL,
    byte_end     INTEGER NOT NULL,
    line_start   INTEGER NOT NULL,
    line_end     INTEGER NOT NULL,
    tokens       INTEGER NOT NULL,
    text_hash    BLOB    NOT NULL CHECK (length(text_hash) = 32),
    UNIQUE (file_id, ord)
) STRICT, WITHOUT ROWID;
CREATE INDEX ix_chunks_text_hash ON chunks(text_hash);
-- Refcount maintained by triggers (also fired by the files -> chunks cascade).
CREATE TRIGGER tr_chunks_ai AFTER INSERT ON chunks BEGIN
    UPDATE chunk_text SET refcount = refcount + 1 WHERE text_hash = NEW.text_hash;
END;
CREATE TRIGGER tr_chunks_ad AFTER DELETE ON chunks BEGIN
    UPDATE chunk_text SET refcount = refcount - 1 WHERE text_hash = OLD.text_hash;
    DELETE FROM chunk_text WHERE text_hash = OLD.text_hash AND refcount <= 0;
END;

-- The outbox. AUTOINCREMENT: a seq is never reused, even after pruning (watermarks rely on it).
CREATE TABLE index_ops (
    seq           INTEGER PRIMARY KEY AUTOINCREMENT,
    collection_id BLOB    NOT NULL,
    profile_id    INTEGER NULL,
    folder_id     BLOB    NULL,
    file_id       BLOB    NULL,
    op            TEXT    NOT NULL CHECK (op IN ('upsert','delete','purge_folder')),
    file_version  INTEGER NOT NULL DEFAULT 0,
    created_at    INTEGER NOT NULL,
    CHECK (op = 'purge_folder' OR file_id IS NOT NULL),
    CHECK (op <> 'purge_folder' OR folder_id IS NOT NULL)
) STRICT;
CREATE INDEX ix_index_ops_collection ON index_ops(collection_id, seq);
CREATE INDEX ix_index_ops_file ON index_ops(file_id, seq) WHERE file_id IS NOT NULL;

-- Cache of each store's own watermark (truth is in the store's commit metadata).
CREATE TABLE store_watermarks (
    store    TEXT    NOT NULL CHECK (store IN ('lance','tantivy')),
    scope_id BLOB    NOT NULL,
    seq      INTEGER NOT NULL,
    PRIMARY KEY (store, scope_id)
) STRICT, WITHOUT ROWID;

CREATE TABLE jobs (
    id           INTEGER PRIMARY KEY,
    kind         TEXT    NOT NULL CHECK (kind IN
                     ('scan_folder','index_file','delete_file','rebuild_profile','purge_folder','retention')),
    folder_id    BLOB    NULL REFERENCES folders(id) ON DELETE CASCADE,
    file_id      BLOB    NULL,
    priority     INTEGER NOT NULL CHECK (priority BETWEEN 0 AND 3),
    dedupe_key   TEXT    NOT NULL,
    payload      TEXT    NULL,
    state        TEXT    NOT NULL DEFAULT 'queued' CHECK (state IN ('queued','leased','done','failed','dead')),
    attempts     INTEGER NOT NULL DEFAULT 0,
    next_run_at  INTEGER NOT NULL,
    lease_owner  INTEGER NULL,
    heartbeat_at INTEGER NULL,
    rerun        INTEGER NOT NULL DEFAULT 0 CHECK (rerun IN (0,1)),
    last_error   TEXT    NULL,
    error_code   TEXT    NULL,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL
) STRICT;
CREATE UNIQUE INDEX ux_jobs_dedupe ON jobs(dedupe_key) WHERE state IN ('queued','leased');
CREATE INDEX ix_jobs_claim  ON jobs(state, priority, next_run_at);
CREATE INDEX ix_jobs_age    ON jobs(state, created_at);
CREATE INDEX ix_jobs_folder ON jobs(folder_id, state);

CREATE TABLE job_history (
    id          INTEGER PRIMARY KEY,
    kind        TEXT    NOT NULL,
    folder_id   BLOB    NULL,
    file_id     BLOB    NULL,
    priority    INTEGER NOT NULL,
    dedupe_key  TEXT    NOT NULL,
    payload     TEXT    NULL,
    state       TEXT    NOT NULL,
    attempts    INTEGER NOT NULL,
    lease_owner INTEGER NULL,
    last_error  TEXT    NULL,
    error_code  TEXT    NULL,
    created_at  INTEGER NOT NULL,
    finished_at INTEGER NOT NULL
) STRICT;
CREATE INDEX ix_job_history_finished ON job_history(finished_at);

-- Stubs for M2–M4 (shape per §2.4; logic lands with their milestones).
CREATE TABLE approvals (
    id         BLOB    PRIMARY KEY CHECK (length(id) = 16),
    kind       TEXT    NOT NULL CHECK (kind IN ('add_folder')),
    client_id  TEXT    NOT NULL,
    payload    TEXT    NOT NULL,
    state      TEXT    NOT NULL DEFAULT 'pending' CHECK (state IN ('pending','approved','denied','expired')),
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
) STRICT;
CREATE INDEX ix_approvals_pending ON approvals(expires_at) WHERE state = 'pending';

CREATE TABLE clients (
    id           TEXT    PRIMARY KEY,
    kind         TEXT    NOT NULL CHECK (kind IN ('claude_desktop','cursor','vscode','other','http')),
    display_name TEXT    NOT NULL,
    scope        TEXT    NOT NULL DEFAULT '"all"',
    token_ref    TEXT    NULL,
    config_path  TEXT    NULL,
    connected_at INTEGER NULL,
    last_seen_at INTEGER NULL
) STRICT;

CREATE TABLE egress_consent (
    provider   TEXT    NOT NULL,
    purpose    TEXT    NOT NULL CHECK (purpose IN
                   ('embed_doc','embed_query','rerank','answer','enhance','iterate','validate_key','list_models')),
    granted_at INTEGER NOT NULL,
    PRIMARY KEY (provider, purpose)
) STRICT, WITHOUT ROWID;

CREATE TABLE egress_audit (
    id            INTEGER PRIMARY KEY,
    ts            INTEGER NOT NULL,
    provider      TEXT    NOT NULL,
    purpose       TEXT    NOT NULL,
    bytes         INTEGER NOT NULL,
    collection_id BLOB    NULL,
    client_id     TEXT    NULL
) STRICT;
CREATE INDEX ix_egress_audit_ts ON egress_audit(ts);
"#;

/// Migration 2 (CO-1): job ids are never reused.
///
/// V1's `jobs.id INTEGER PRIMARY KEY` restarts at `max(id) + 1`, i.e. at 1 again once the queue
/// drains (finished jobs move to `job_history`). The next job then collided with an archived one,
/// and `to_history`'s `ON CONFLICT(id)` kept the old row's kind and payload: history (and every
/// consumer of job ids) silently lost or mislabelled jobs. `AUTOINCREMENT` makes ids monotonic;
/// the sequence starts past every id already in either table.
const V2: &str = r#"
CREATE TABLE jobs_v2 (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    kind         TEXT    NOT NULL CHECK (kind IN
                     ('scan_folder','index_file','delete_file','rebuild_profile','purge_folder','retention')),
    folder_id    BLOB    NULL REFERENCES folders(id) ON DELETE CASCADE,
    file_id      BLOB    NULL,
    priority     INTEGER NOT NULL CHECK (priority BETWEEN 0 AND 3),
    dedupe_key   TEXT    NOT NULL,
    payload      TEXT    NULL,
    state        TEXT    NOT NULL DEFAULT 'queued' CHECK (state IN ('queued','leased','done','failed','dead')),
    attempts     INTEGER NOT NULL DEFAULT 0,
    next_run_at  INTEGER NOT NULL,
    lease_owner  INTEGER NULL,
    heartbeat_at INTEGER NULL,
    rerun        INTEGER NOT NULL DEFAULT 0 CHECK (rerun IN (0,1)),
    last_error   TEXT    NULL,
    error_code   TEXT    NULL,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL
) STRICT;
INSERT INTO jobs_v2(id, kind, folder_id, file_id, priority, dedupe_key, payload, state, attempts,
        next_run_at, lease_owner, heartbeat_at, rerun, last_error, error_code, created_at, updated_at)
    SELECT id, kind, folder_id, file_id, priority, dedupe_key, payload, state, attempts,
        next_run_at, lease_owner, heartbeat_at, rerun, last_error, error_code, created_at, updated_at
    FROM jobs;
DROP TABLE jobs;
ALTER TABLE jobs_v2 RENAME TO jobs;
CREATE UNIQUE INDEX ux_jobs_dedupe ON jobs(dedupe_key) WHERE state IN ('queued','leased');
CREATE INDEX ix_jobs_claim  ON jobs(state, priority, next_run_at);
CREATE INDEX ix_jobs_age    ON jobs(state, created_at);
CREATE INDEX ix_jobs_folder ON jobs(folder_id, state);
DELETE FROM sqlite_sequence WHERE name IN ('jobs', 'jobs_v2');
INSERT INTO sqlite_sequence(name, seq) VALUES ('jobs', max(
    coalesce((SELECT max(id) FROM jobs), 0),
    coalesce((SELECT max(id) FROM job_history), 0)));
"#;

/// Migration 3 (M2-W): live sync and approvals.
///
/// * `folders.watch_support`: whether the notify watcher can serve the folder (`native`), or it is
///   reconcile-only because it is on a network/mapped drive (`network`) or the watcher could not be
///   started on its file system (`unsupported`). The UI shows a plain note for the last two.
/// * `folders.paused_at` / `folders.cancelled_at`: pause and cancel per folder. The claim **holds**
///   jobs of paused folders (no attempt is consumed, nothing is dropped). `cancelled_at` marks a
///   run the user stopped; it clears when the next full scan finishes.
/// * `approvals`: the M1 stub gains what the approve/deny flow needs (client kind for the
///   `ApprovalRequested` event, a dedupe key so a client asking twice gets the same request, the
///   decision time and its result).
/// * Files-only listing: index for the "listed" vs "skipped" split (`skip_reason = 'files_only'`).
/// * No DDL for the 64-bit volume serial: `INTEGER` is already 64-bit. Values are stored as the
///   serial's bit pattern in an `i64` (see `model::vs_to_sql`). Rows written before V3 hold the low
///   32 bits; sr-sync upgrades a folder (and its files) in place the first time it sees the full
///   serial with matching low bits (`folders::upgrade_volume_serial`).
const V3: &str = r#"
ALTER TABLE folders ADD COLUMN watch_support TEXT NOT NULL DEFAULT 'native'
    CHECK (watch_support IN ('native','network','unsupported'));
ALTER TABLE folders ADD COLUMN paused_at    INTEGER NULL;
ALTER TABLE folders ADD COLUMN cancelled_at INTEGER NULL;
UPDATE folders SET paused_at = updated_at WHERE paused = 1;

CREATE INDEX ix_files_skip ON files(folder_id, skip_reason) WHERE state = 'skipped';

ALTER TABLE approvals ADD COLUMN client_kind TEXT NOT NULL DEFAULT 'other'
    CHECK (client_kind IN ('claude_desktop','cursor','vs_code','other','http'));
ALTER TABLE approvals ADD COLUMN target_key TEXT    NOT NULL DEFAULT '';
ALTER TABLE approvals ADD COLUMN decided_at INTEGER NULL;
ALTER TABLE approvals ADD COLUMN folder_id  BLOB    NULL CHECK (folder_id IS NULL OR length(folder_id) = 16);
ALTER TABLE approvals ADD COLUMN error_code TEXT    NULL;
CREATE UNIQUE INDEX ux_approvals_pending_target ON approvals(kind, client_id, target_key)
    WHERE state = 'pending';
CREATE INDEX ix_approvals_created ON approvals(created_at);
"#;

/// Migration 4 (M4-E): folder groups, connected apps, activity.
///
/// * `folder_groups`: a **label over folders** (page 31). Per-app scopes name groups. Every group
///   shares the one search index (the default store collection); moving a folder between groups
///   never touches the stores. `folders.group_id` NULL = the default group ("My folders").
/// * `clients`: `token_hint` holds an HTTP tool token's last 4 characters (the token itself is
///   only in Credential Manager; `token_ref` names its credential).
/// * `activity_log`: page 30 lines that no other table records (folder added / removed, search
///   upgraded, updates, …). 7-day retention. `AUTOINCREMENT` keeps ids monotonic for paging.
const V4: &str = r#"
CREATE TABLE folder_groups (
    id         BLOB    PRIMARY KEY CHECK (length(id) = 16),
    name       TEXT    NOT NULL COLLATE NOCASE UNIQUE,
    is_default INTEGER NOT NULL DEFAULT 0 CHECK (is_default IN (0,1)),
    created_at INTEGER NOT NULL
) STRICT;
CREATE UNIQUE INDEX ux_folder_groups_default ON folder_groups(is_default) WHERE is_default = 1;

ALTER TABLE folders ADD COLUMN group_id BLOB NULL REFERENCES folder_groups(id) ON DELETE SET NULL;
CREATE INDEX ix_folders_group ON folders(group_id) WHERE group_id IS NOT NULL;

ALTER TABLE clients ADD COLUMN token_hint TEXT NULL;

CREATE TABLE activity_log (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    ts          INTEGER NOT NULL,
    kind        TEXT    NOT NULL,
    folder_id   BLOB    NULL CHECK (folder_id IS NULL OR length(folder_id) = 16),
    folder_name TEXT    NULL,
    client_kind TEXT    NULL,
    client_name TEXT    NULL,
    count       INTEGER NULL,
    text        TEXT    NULL
) STRICT;
CREATE INDEX ix_activity_log_ts ON activity_log(ts);
"#;

/// Migration 5 (Amendment 17): the Files-only folder mode is removed.
///
/// A folder still in it must never start being read without the user's say-so, so it is
/// **paused** (the claim holds its jobs, no scan or watcher runs) and keeps
/// `indexing_mode = 'files_only'` as the mark the app shows its question for
/// (`folders::FolderRecord::was_files_only`): search its contents (resume, which clears the mark)
/// or remove it. Its files stay listed (`skip_reason = 'files_only'`) until then. Access grants
/// were never read either way; their column goes back to the default. No DDL: `indexing_mode`
/// keeps its V1 column and CHECK, and is no longer a setting.
const V5: &str = r#"
UPDATE folders SET paused = 1, paused_at = coalesce(paused_at, updated_at)
    WHERE indexing_mode = 'files_only' AND kind = 'indexed';
UPDATE folders SET indexing_mode = 'vector'
    WHERE indexing_mode = 'files_only' AND kind <> 'indexed';
"#;

/// All migrations, oldest first.
pub fn migrations() -> Migrations<'static> {
    Migrations::new(vec![
        M::up(V1).foreign_key_check(),
        M::up(V2).foreign_key_check(),
        M::up(V3).foreign_key_check(),
        M::up(V4).foreign_key_check(),
        M::up(V5).foreign_key_check(),
    ])
}

/// Schema version after all migrations.
pub const LATEST_VERSION: usize = 5;

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;

    /// A V1 catalog upgrades in place: live jobs survive, and new ids start past every id in
    /// `jobs` and `job_history` (so an archived job is never shadowed).
    #[test]
    fn v2_upgrades_a_v1_catalog_and_never_reuses_job_ids() {
        let mut c = Connection::open_in_memory().unwrap();
        c.pragma_update(None, "foreign_keys", "ON").unwrap();
        migrations().to_version(&mut c, 1).unwrap();
        c.execute_batch(
            "INSERT INTO jobs(id, kind, priority, dedupe_key, payload, next_run_at, created_at, updated_at)
                 VALUES (7, 'retention', 3, 'retention:global', '{\"x\":1}', 1, 1, 1);
             INSERT INTO job_history(id, kind, priority, dedupe_key, state, attempts, created_at, finished_at)
                 VALUES (9, 'index_file', 1, 'index_file:file:a', 'done', 1, 1, 2);",
        )
        .unwrap();
        migrations().to_latest(&mut c).unwrap();
        let v: i64 = c
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, i64::try_from(LATEST_VERSION).unwrap());
        let kept: (String, String) = c
            .query_row("SELECT kind, payload FROM jobs WHERE id = 7", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(kept, ("retention".into(), "{\"x\":1}".into()));
        c.execute("DELETE FROM jobs", []).unwrap();
        let id: i64 = c
            .query_row(
                "INSERT INTO jobs(kind, priority, dedupe_key, next_run_at, created_at, updated_at)
                     VALUES ('retention', 3, 'retention:global', 1, 1, 1) RETURNING id",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(id, 10, "past max(jobs.id = 7, job_history.id = 9)");
        let idx: i64 = c
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'index' AND tbl_name = 'jobs'
                   AND name IN ('ux_jobs_dedupe','ix_jobs_claim','ix_jobs_age','ix_jobs_folder')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(idx, 4, "indexes recreated");
    }

    /// A V2 catalog upgrades to V3 in place: folders, files, queued jobs and a pending approval
    /// survive; new columns get their defaults (a paused folder keeps its pause, with a time).
    #[test]
    fn v3_upgrades_a_v2_catalog_in_place() {
        let mut c = Connection::open_in_memory().unwrap();
        c.pragma_update(None, "foreign_keys", "ON").unwrap();
        migrations().to_version(&mut c, 2).unwrap();
        let col = [1u8; 16];
        let fol = [2u8; 16];
        let paused = [3u8; 16];
        c.execute_batch(&format!(
            "INSERT INTO embedding_profiles(id, provider, model, model_revision, dims, pooling, variant_path,
                 variant_sha256, precision, tier, fingerprint, created_at)
                 VALUES (1, 'local', 'm', 'r', 4, 'cls', 'p', 's', 'fp32', 'minimum', x'{fp}', 1);
             INSERT INTO collections(id, name, embedding_profile_id, created_at) VALUES (x'{col}', 'c', 1, 1);
             INSERT INTO folders(id, collection_id, kind, root_path, root_key, volume_serial, created_at, updated_at)
                 VALUES (x'{fol}', x'{col}', 'indexed', 'C:\\a', 'c:/a', 305419896, 1, 1);
             INSERT INTO folders(id, collection_id, kind, root_path, root_key, paused, created_at, updated_at)
                 VALUES (x'{paused}', x'{col}', 'indexed', 'C:\\b', 'c:/b', 1, 1, 42);
             INSERT INTO files(id, folder_id, rel_path, path_key, dir_key, volume_serial, size, mtime_ns,
                 state, skip_reason, created_at, updated_at)
                 VALUES (x'{f1}', x'{fol}', 'a.md', 'a.md', '', 305419896, 1, 1, 'skipped', 'files_only', 1, 1);
             INSERT INTO jobs(kind, folder_id, priority, dedupe_key, payload, next_run_at, created_at, updated_at)
                 VALUES ('scan_folder', x'{fol}', 1, 'scan_folder:folder:x', '{{\"trigger\":\"manual\"}}', 1, 1, 1);
             INSERT INTO approvals(id, kind, client_id, payload, created_at, expires_at)
                 VALUES (x'{ap}', 'add_folder', 'claude', '{{}}', 1, 2);",
            fp = "ab".repeat(32),
            col = hex(&col),
            fol = hex(&fol),
            paused = hex(&paused),
            f1 = hex(&[4u8; 16]),
            ap = hex(&[5u8; 16]),
        ))
        .unwrap();
        migrations().to_latest(&mut c).unwrap();
        let v: i64 = c
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, i64::try_from(LATEST_VERSION).unwrap());
        let (ws, serial): (String, i64) = c
            .query_row(
                "SELECT watch_support, volume_serial FROM folders WHERE id = ?1",
                [fol],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((ws.as_str(), serial), ("native", 305_419_896));
        let paused_at: Option<i64> = c
            .query_row(
                "SELECT paused_at FROM folders WHERE id = ?1",
                [paused],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(paused_at, Some(42), "an M1 pause keeps holding");
        let jobs: i64 = c
            .query_row(
                "SELECT count(*) FROM jobs WHERE state = 'queued'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(jobs, 1);
        let (kind, key, state): (String, String, String) = c
            .query_row(
                "SELECT client_kind, target_key, state FROM approvals",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (kind.as_str(), key.as_str(), state.as_str()),
            ("other", "", "pending")
        );
        // The new CHECK constraints hold.
        assert!(
            c.execute("UPDATE folders SET watch_support = 'sometimes'", [])
                .is_err()
        );
        assert!(
            c.execute("UPDATE approvals SET client_kind = 'vscode'", [])
                .is_err()
        );
    }

    /// A V3 catalog upgrades to V4 in place: folders keep their rows with no group (= the default
    /// group), clients keep their rows, and the new tables exist and accept rows.
    #[test]
    fn v4_upgrades_a_v3_catalog_in_place() {
        let mut c = Connection::open_in_memory().unwrap();
        c.pragma_update(None, "foreign_keys", "ON").unwrap();
        migrations().to_version(&mut c, 3).unwrap();
        let col = [1u8; 16];
        let fol = [2u8; 16];
        c.execute_batch(&format!(
            "INSERT INTO embedding_profiles(id, provider, model, model_revision, dims, pooling, variant_path,
                 variant_sha256, precision, tier, fingerprint, created_at)
                 VALUES (1, 'local', 'm', 'r', 4, 'cls', 'p', 's', 'fp32', 'minimum', x'{fp}', 1);
             INSERT INTO collections(id, name, embedding_profile_id, created_at) VALUES (x'{col}', 'c', 1, 1);
             INSERT INTO folders(id, collection_id, kind, root_path, root_key, created_at, updated_at)
                 VALUES (x'{fol}', x'{col}', 'indexed', 'C:\\a', 'c:/a', 1, 1);
             INSERT INTO clients(id, kind, display_name) VALUES ('claude', 'claude_desktop', 'Claude');",
            fp = "cd".repeat(32),
            col = hex(&col),
            fol = hex(&fol),
        ))
        .unwrap();
        migrations().to_latest(&mut c).unwrap();
        let g: Option<Vec<u8>> = c
            .query_row("SELECT group_id FROM folders WHERE id = ?1", [fol], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(g.is_none(), "existing folders stay in the default group");
        let hint: Option<String> = c
            .query_row(
                "SELECT token_hint FROM clients WHERE id = 'claude'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(hint.is_none());
        c.execute(
            "INSERT INTO folder_groups(id, name, is_default, created_at) VALUES (?1, 'My folders', 1, 1)",
            [[7u8; 16]],
        )
        .unwrap();
        assert!(
            c.execute(
                "INSERT INTO folder_groups(id, name, is_default, created_at) VALUES (?1, 'MY FOLDERS', 0, 1)",
                [[8u8; 16]],
            )
            .is_err(),
            "group names are unique, case-insensitive"
        );
        assert!(
            c.execute(
                "INSERT INTO folder_groups(id, name, is_default, created_at) VALUES (?1, 'Other', 1, 1)",
                [[9u8; 16]],
            )
            .is_err(),
            "only one default group"
        );
        c.execute("UPDATE folders SET group_id = ?1", [[7u8; 16]])
            .unwrap();
        // Deleting a group never deletes folders (SET NULL = back to the default group).
        c.execute("DELETE FROM folder_groups", []).unwrap();
        let n: i64 = c
            .query_row(
                "SELECT count(*) FROM folders WHERE group_id IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
        let id: i64 = c
            .query_row(
                "INSERT INTO activity_log(ts, kind) VALUES (1, 'folder_added') RETURNING id",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(id, 1);
    }

    /// Amendment 17: a V4 catalog upgrades to V5 in place. A Files-only folder is paused and
    /// keeps its mark and its listed files (nothing is read); a pause the user already set keeps
    /// its time; searched folders and access grants are never paused.
    #[test]
    fn v5_pauses_files_only_folders_and_keeps_the_mark() {
        let mut c = Connection::open_in_memory().unwrap();
        c.pragma_update(None, "foreign_keys", "ON").unwrap();
        migrations().to_version(&mut c, 4).unwrap();
        let col = [1u8; 16];
        let (fo, fo_paused, vector, grant) = ([2u8; 16], [3u8; 16], [4u8; 16], [5u8; 16]);
        c.execute_batch(&format!(
            "INSERT INTO embedding_profiles(id, provider, model, model_revision, dims, pooling, variant_path,
                 variant_sha256, precision, tier, fingerprint, created_at)
                 VALUES (1, 'local', 'm', 'r', 4, 'cls', 'p', 's', 'fp32', 'minimum', x'{fp}', 1);
             INSERT INTO collections(id, name, embedding_profile_id, created_at) VALUES (x'{col}', 'c', 1, 1);
             INSERT INTO folders(id, collection_id, kind, root_path, root_key, indexing_mode, created_at, updated_at)
                 VALUES (x'{fo}', x'{col}', 'indexed', 'C:\\a', 'c:/a', 'files_only', 1, 7);
             INSERT INTO folders(id, collection_id, kind, root_path, root_key, indexing_mode, paused, paused_at,
                 created_at, updated_at)
                 VALUES (x'{fo_paused}', x'{col}', 'indexed', 'C:\\b', 'c:/b', 'files_only', 1, 3, 1, 9);
             INSERT INTO folders(id, collection_id, kind, root_path, root_key, created_at, updated_at)
                 VALUES (x'{vector}', x'{col}', 'indexed', 'C:\\c', 'c:/c', 1, 1);
             INSERT INTO folders(id, collection_id, kind, root_path, root_key, indexing_mode, created_at, updated_at)
                 VALUES (x'{grant}', x'{col}', 'access_grant', 'C:\\d', 'c:/d', 'files_only', 1, 1);
             INSERT INTO files(id, folder_id, rel_path, path_key, dir_key, size, mtime_ns, state, skip_reason,
                 created_at, updated_at)
                 VALUES (x'{f1}', x'{fo}', 'a.md', 'a.md', '', 1, 1, 'skipped', 'files_only', 1, 1);",
            fp = "ef".repeat(32),
            col = hex(&col),
            fo = hex(&fo),
            fo_paused = hex(&fo_paused),
            vector = hex(&vector),
            grant = hex(&grant),
            f1 = hex(&[6u8; 16]),
        ))
        .unwrap();
        migrations().to_latest(&mut c).unwrap();
        let v: i64 = c
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, i64::try_from(LATEST_VERSION).unwrap());
        let row = |id: [u8; 16]| -> (bool, Option<i64>, String) {
            c.query_row(
                "SELECT paused, paused_at, indexing_mode FROM folders WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap()
        };
        assert_eq!(
            row(fo),
            (true, Some(7), "files_only".into()),
            "paused, marked"
        );
        assert_eq!(
            row(fo_paused),
            (true, Some(3), "files_only".into()),
            "the user's own pause keeps its time"
        );
        assert_eq!(row(vector), (false, None, "vector".into()), "untouched");
        assert_eq!(
            row(grant),
            (false, None, "vector".into()),
            "never asked about"
        );
        let (state, reason): (String, String) = c
            .query_row("SELECT state, skip_reason FROM files", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((state.as_str(), reason.as_str()), ("skipped", "files_only"));
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
}
