# 02. Target architecture

> Status: implemented (v0.1.0). This is the design record.
> Binding: Product Principles P1 (average-user simplicity, never weakening security) and P2 (standing check), see `product-principles.md`.

## Product Principles (summary; full text in `product-principles.md`)

- **P1, average-user simplicity (core):** a non-technical user installs, sets up and uses the product without documentation.
  - One installer (releases are not code-signed yet; signing is planned). No terminal, no config edits, no extra runtimes.
  - Working defaults: local embeddings, no key needed. Optional features are off.
  - A first-run wizard. One-click or copy-paste MCP Connect.
  - Plain-language errors, each with one next action. Push-based progress.
  - Advanced settings are never required.
- **Security is never weakened.** When simplicity and security conflict, the secure default wins and stays invisible to the user.
- **P2 standing check:** "Can an average user do this without documentation?" Every failure is logged as an `R-UX` risk in `04-roadmap.md`.

How P1 shows up in this architecture:
- No TCP port and no token by default.
- The background service is invisible.
- One app-written `settings.toml` with generated Basic and Advanced forms (§2.5).
- An `ErrorCode` → `UserMessage` table (§2.5).
- Every security control is on by default with no user action (§2.6).

---

## 2.1 Crate layout

Your five crates (`core`, `providers`, `sync`, `mcp-server`, `app`) are kept with the same responsibilities. Six crates are added, each for a reason you can check in the dependency graph.

| Crate (package) | Kind | Contents | Why it exists |
|---|---|---|---|
| `common` (`sr-common`) | lib | Config schema (`Settings`, `FolderSettings`), `ErrorCode`/`Error`/`UserMessage`, `EngineEvent`/`EngineStatus` + `Seq`, IDs, API DTOs, the **daemon protocol** (handshake, app-RPC envelopes), the extract-worker protocol, `sandbox` (path keys, rel-path validation, dangerous roots, SecretFilter) | Holds the **one** schema, **one** error model and **one** event type set. Small: no tokio-heavy or storage dependencies. |
| `catalog` (`sr-catalog`) | lib | rusqlite writer and read pool, migrations, repositories, outbox, jobs and claim SQL, approvals, clients, consent and audit | Source of truth. Used by both sync and core; tests run without compiling LanceDB. |
| `providers` (`sr-providers`) | lib | `LlmProvider`, `EmbeddingProvider`, `Reranker` traits; `Resilient<P>`; fastembed local; OpenAI-compatible, Gemini and Ollama embed; genai LLM; keyring `SecretStore`; model downloader | **One** provider interface. |
| `core` (`sr-core`) | lib | `ingest` (extract client supervising `sr-extract` workers; chunk; EmbedBatcher), `index` (lance, tantivy, IndexWriter, outbox apply, replay, purge), `query` (dense, bm25, rrf, rerank, mmr, hydrate), `answer`, `enhance`, `iterate`, `files` (read_file, grep, tree, path search) | Your "core". Ingest, index and query are modules, not crates: they share LanceDB types, and splitting them would not cut build time. |
| `sync` (`sr-sync`) | lib | walker, reconciler, watcher, policy (`FolderBehavior`), scheduler, claim loop, `trait JobExecutor` | Depends on catalog only; engine wires core's executor in. |
| `engine` (`sr-engine`) | lib | Composition root: `Engine::start` (lock → migrate → epoch → open stores → replay → re-validate roots → reconcile → watchers → ready), `EngineHandle`, event bus, pipe server, app-RPC service | One composition root. |
| `mcp-server` (`sr-mcp-server`) | lib + **bin `sr-daemon`** | rmcp tools, resources and prompts over `EngineHandle`; MCP over the pipe (async-rw); opt-in streamable HTTP with a bearer layer; hidden `sr-daemon admin …` CLI | Your "mcp-server": the MCP server *is* the daemon. |
| `client` (`sr-client`) | lib | Pipe client: attach (connect → spawn.lock → task run → wait), handshake, app-RPC client, event subscription with resume | Shared by bridge and app; no engine dependencies. |
| `bridge` (`sr-bridge`) | bin | stdio ↔ pipe byte proxy, about 300 LOC | **Must not link LanceDB, ort or pdfium.** A separate package is the only reliable way, because bins in one package share its dependency graph. |
| `extract` (`sr-extract`) | bin | Parsers only; frames over stdin/stdout | Process isolation; pdfium only lives here. |
| `app` (`sr-app`) | bin (Tauri) | UI, tray, wizard, Connect writers, updater; commands forward to `sr-client` | Your "app": a client only. |
| `xtask` | bin | specta bindings, JSON Schema export, packaging, golden-set eval runner | Dev only. |

Changed 2026-11-08 (Amendment 16): AI apps now see `context_search`, `list_folders`, `context_answer`, and `add_folder` / `sync_folder` only when the user turns them on; a tool that is not listed answers like an unknown tool.

Dependency graph (acyclic, enforced by Cargo):
```
common ◄── catalog ◄─────────────┐
   ▲  ◄── providers ◄── core ◄───┤
   │           ▲         ▲       │
   │           └─────────┴── engine ◄── mcp-server (bin sr-daemon)
   │  ◄── sync ◄──────────────────┘
   │  ◄── client ◄── bridge (bin)
   │            ◄── app (bin)
   └── extract (bin)
core → {common, catalog, providers};  sync → {common, catalog};  engine → all libs
```

## 2.2 Process topology

```
 MCP client (Claude Desktop [MSIX], Cursor, VS Code)    sr-app.exe (Tauri UI / tray)     Other local tools (opt-in)
        │ launches with stdio                                │                                │ HTTP 127.0.0.1:<port>
        ▼                                                    │ app-RPC (JSON-RPC 2.0,         │ Bearer token per client,
   sr-bridge.exe --client claude                             │  length-delimited frames)      │ allowed_hosts=loopback,
        │ 1 connect pipe → handshake{proto, build, kind=mcp, client_id}                      │ Origin check, limits
        │ 2 if absent: LockFileEx(spawn.lock) → re-connect                                    │ (off by default)
        │ 3 schtasks /run /tn "\SR\daemon-<sid8>"  (per user, 4.6; escapes Job + MSIX ctx)       │
        │ 4 fallback: CreateProcess(DETACHED|BREAKAWAY_FROM_JOB) if not packaged; else in-job │
        │ 5 wait for pipe ≤ 10 s, then copy bytes both ways                                   │
        ▼                                                    ▼                                ▼
   \\.\pipe\selene-retrieval-<blake3(user SID)[..16]>   (DACL: user SID only; reject remote; FIRST_PIPE_INSTANCE)
        ▼
   sr-daemon.exe  — sole owner of %LOCALAPPDATA%\SeleneRetrieval (protected DACL); LockFileEx(host.lock)
     ├─ refuses to own stores if GetCurrentPackageFullName succeeds (packaged context) → exits; client shows U8 text
     ├─ sr-mcp-server: one rmcp session per MCP connection (+ HTTP sessions when enabled)
     ├─ app-RPC: typed API + subscribe(from_seq) event stream
     ├─ sr-engine → catalog | core | sync | providers
     └─ sr-extract.exe ×N  (nested Job Object: 1 GiB, kill-on-close; low IL; no FS; bytes over anonymous pipe)

 Lifecycle: "Start with Windows" → HKCU Run: sr-app --background (tray only) → attach (same steps as bridge).
            Autostart off → daemon exits 10 min after last client, empty queue and empty writer backlog.
            Update: updater → app-RPC shutdown{drain} → daemon finishes the current batch, commits, exits → install → relaunch.
            Approval with app closed: daemon launches sr-app --approve <id>.
```

Handshake: `{proto: u16, build: semver, kind: mcp|app, client_id}`. The daemon accepts `proto ∈ {N-1, N}`. For `kind=mcp`, the rest of the stream is raw MCP JSON-RPC, so MCP version negotiation happens end to end between client and daemon.

## 2.3 Data flow

```
 notify (debounce 500 ms) ─┐
 scheduler (reconcile 60m) ┼─► Reconciler / ChangeDetector ──► jobs (SQLite, dedupe_key, priority, epoch lease)
 UI / MCP / startup ───────┘     (file_id,size,mtime → blake3)          │ claim loop: DRR across folders, aging,
                                  scan_dirs.complete gates deletes      │ permits = downstream capacity (backpressure)
                                                                        ▼
  core::ingest: read via SandboxFs ─► [txt/md/code: in-proc decode | pdf/docx/xlsx/html: sr-extract worker]
     ─► chunk (text-splitter, chunk_cfg_fp) ─► diff text_hash vs catalog
     ─► T1 (catalog writer): files.version++, chunks, chunk_text(refcount), index_ops(seq) ; files.state=pending_index
     ─► EmbedBatcher (per profile; token budget / 50 ms max-wait; Lance text_hash lookup → copy vectors; governor)
     ─► IndexWriter (single task, group commit 2 s / 512 chunks / 1k ops):
            Lance: delete file_id + merge_insert(chunk_id) → commit → write watermark(seq)
            tantivy[collection]: delete_term(file_id) + add → commit(payload=seq)
     ─► T2: files.state=indexed ; prune index_ops ≤ min(watermarks)
     ─► EventBus: Seq<EngineEvent> ──► app-RPC subscribers (Tauri Channel) │ MCP progress + resources/updated

  Startup replay: for each store, ops with seq > watermark, coalesced per file_id → re-apply (idempotent)

  Query: search(q, scope) → resolve scope (client scope ∩ request) → query-embed (moka LRU)
     ┬─ Lance ANN (IVF_HNSW_SQ, filter collection_id/folder_id via BITMAP; flat if selectivity <5%)
     └─ tantivy BM25 per collection (CodeTokenizer, path boost)
     → RRF(k) → drop stale chunk_ids (not in current file version) → [rerank top 50] → [MMR] → min_score
     → hydrate text + lines from catalog → hits {path, line range, snippet, scores{rrf, dense, bm25, rerank}}
     → optional: answer (search 3× its passage budget, pick file by file, docs/08; structured JSON; bounded read_file loop in scope) | iterate (≤2 rounds) | enhance_prompt (the app's Enhance page through app-RPC, not an MCP tool)
```

## 2.4 Catalog schema (SQLite, source of truth)

```
meta(key PK, value)                              -- schema_version, daemon_epoch, install_id,
                                                 -- hw_detection JSON (last HwInfo + TierDecision); detected facts only:
                                                 -- tier_override is a user choice in settings.toml (4.10)
collections(id ULID PK, name, embedding_profile_id FK, active_index_id FK NULL → vector_indexes, is_default, created_at)
embedding_profiles(id PK, provider, model, model_revision, dims, pooling, query_prefix, doc_prefix,
                   variant_path, variant_sha256, precision ENUM(fp32|fp16|int8|remote), tier ENUM(minimum|standard|high|custom),
                   fingerprint UNIQUE,  -- blake3(provider‖model‖revision‖variant_sha256‖dims‖pooling‖prefixes)
                   is_remote, state ENUM(active|building|retired))
vector_indexes(id ULID PK, collection_id FK CASCADE, profile_id FK, dims, lance_table,      -- M1 (Amendment 1)
               state ENUM(building|active|retired|failed), rows_total, rows_done,
               checkpoint_after_chunk_id BLOB(32) NULL, start_seq,      -- resume point + outbox seq at start
               failure_code NULL, created_at, activated_at NULL, retired_at NULL,
               UNIQUE(collection_id) WHERE state='active', UNIQUE(collection_id) WHERE state='building')
model_files(sha256 PK, repo, revision, path, bytes, bytes_have, state ENUM(partial|verified|bundled), updated_at)  -- M2 resume
folders(id ULID PK, collection_id FK CASCADE, kind ENUM(indexed|access_grant), display_name, is_primary,
        root_path, root_key UNIQUE(collection_id, root_key), volume_serial, recursive=1,
        indexing_mode ENUM(files_only|vector), sync_mode ENUM(auto|manual|scheduled|triggered),  -- indexing_mode 2026-11-08: removed, Amendment 17 (no longer a setting; 'files_only' marks a folder paused until the user chooses)
        cadence_min=60, reindex_policy ENUM(smart|always|never)=smart,
        file_types_preset ENUM(documents|code|documents_and_code|all_text|custom)=documents_and_code,  -- 2026-11-09: code adds GDScript (.gd), Godot shaders, .tscn, project.godot and 89 more code types (DocKind::from_extension); code leaves out non-config JSON and .tres (sr_sync::filter::is_code_data_file); .godot/, .import/, *.import, *.uid are default ignores; IGNORE_DEFAULTS_VERSION 3 rescans existing folders
        include_extensions JSON, exclude_globs JSON, file_type_filters JSON,
        max_file_bytes=10485760, chunk_preset ENUM(balanced|small|large|custom)=balanced,
        chunk_size_override NULL, chunk_overlap_override NULL, follow_symlinks=0, include_online_only=0,
        status, status_reason_code, generation, last_full_scan_at, last_success_at, last_run_trigger,
        stats_json, last_run_metadata JSON, inherited_from_folder_id NULL, provenance JSON NULL,
        expires_at NULL, created_at, updated_at)
scan_dirs(scan_id, folder_id FK CASCADE, rel_dir_key, complete BOOL, error_code NULL, PK(scan_id, folder_id, rel_dir_key))
files(id ULID PK, folder_id FK CASCADE, rel_path, path_key, ext, volume_serial, ntfs_file_id BLOB(16),
      size, mtime_ns, content_hash, version, extractor_version, chunk_cfg_fp, scan_id,
      state ENUM(pending_index|indexed|skipped|dead), skip_reason NULL, error_code NULL, attempts,
      UNIQUE(folder_id, path_key), INDEX(volume_serial, ntfs_file_id))
files_fts  FTS5(rel_path, content='files')       -- @-picker / files.find
chunks(chunk_id BLOB(32) PK, file_id FK CASCADE, file_version, ord, byte_start, byte_end,
       line_start, line_end, tokens, text_hash, INDEX(file_id), INDEX(text_hash))
chunk_text(text_hash PK, zstd BLOB, bytes, refcount)           -- refcount maintained by triggers on chunks
index_ops(seq INTEGER PK AUTOINCREMENT, collection_id, profile_id, file_id, op ENUM(upsert|delete|purge_folder),
          file_version, created_at)
store_watermarks(store ENUM(lance|tantivy), scope_id, seq, PK(store, scope_id))  -- cache; truth is in the store
jobs(id PK AUTOINCREMENT (never reused: schema V2, CO-1), kind ENUM(scan_folder|index_file|delete_file|rebuild_profile|purge_folder|retention),
     folder_id FK CASCADE NULL, file_id NULL, priority 0..3, dedupe_key, payload JSON,
     state ENUM(queued|leased|done|failed|dead), attempts, next_run_at, lease_owner(epoch),
     last_error, error_code, created_at, updated_at,
     UNIQUE(dedupe_key) WHERE state IN ('queued','leased'),
     INDEX(state, priority, next_run_at), INDEX(folder_id, state))
job_history(... same columns, finished_at)       -- 7-day retention
approvals(id ULID PK, kind ENUM(add_folder), client_id, payload JSON, state ENUM(pending|approved|denied|expired),
          created_at, expires_at)
clients(id PK, kind ENUM(claude_desktop|cursor|vscode|other|http), display_name, scope JSON, token_ref NULL,
        config_path NULL, connected_at, last_seen_at)
egress_consent(provider, purpose, granted_at, PK(provider, purpose))
egress_audit(ts, provider, purpose, bytes, collection_id, client_id)
-- V4 (M4): folder groups = labels over folders (page 31); per-app scopes name groups; page 30 activity
folder_groups(id ULID PK, name UNIQUE COLLATE NOCASE, is_default UNIQUE WHERE is_default=1, created_at)
folders.group_id  FK NULL → folder_groups ON DELETE SET NULL   -- NULL = the default group "My folders"
clients.token_hint NULL                          -- last 4 chars of an HTTP tool token; the token is only in
                                                 -- Credential Manager (SeleneRetrieval/http-token:<client_id>)
activity_log(id INTEGER PK AUTOINCREMENT, ts, kind, folder_id NULL, folder_name NULL, client_kind NULL,
             client_name NULL, count NULL, text NULL, INDEX(ts))   -- 7-day retention (maintenance pass)
```

V4 notes: every group shares the one search index and model (the default store collection), so moving a folder between groups never touches the stores (M4-E D1). A scope that names an unknown or deleted group grants nothing (fail closed). Outside the catalog, the anchor `%LOCALAPPDATA%\SeleneRetrieval` holds `index-location.json` (pointer to a moved index root: `catalog.db*`, `lance\`, `tantivy\`, `models\`) and `index-move.json` (a pending move plan); settings, logs and locks never move (M4-E D2).

Integrity rules:
- Folder status is derived from jobs and files: `synced` only when no queued, leased or failed jobs remain at the current generation; `synced_with_errors` when the only problems are dead files.
- Deletes happen only for directories with `scan_dirs.complete=1` (mark and sweep; W4). Partial subtrees are marked `partial` and their files are kept.
- A `chunk_cfg_fp` change queues priority-3 `index_file` jobs for the files it affects.
- An embedding profile change (model, tier or manual override) is a shadow rebuild: a new `building` row in `vector_indexes` with its own Lance table, swapped in atomically when complete (W16, §2.8).
- **Every vector index records its model and dims** (`vector_indexes.profile_id` → model, revision, variant sha256, dims). Queries are embedded with *the active index's* profile, never with the current tier's. A query whose model or vector length differs from the index's profile is refused with `ModelIncompatible` (logged; `sr_providers` returns it); it is never compared.
- Chunking is tier-independent. `chunk_cfg_fp` uses one fixed chunker tokenizer (bert-base-uncased WordPiece, shared by every tier model; verified M0-E), so **a tier or model change never changes chunk IDs**.

## 2.5 Cross-cutting contracts (one instance of each)

### Provider interface (`sr-providers`)
```rust
trait EmbeddingProvider { fn profile(&self) -> &EmbeddingProfile; fn max_batch_tokens(&self) -> usize;
    async fn embed_batch(&self, kind: EmbedKind /*Doc|Query*/, texts: &[&str], cancel: &CancellationToken) -> Result<Vec<Vec<f32>>>; }
trait LlmProvider { fn capabilities(&self) -> Caps; async fn list_models(&self) -> Result<Vec<ModelInfo>>;
    async fn chat(&self, req: ChatRequest, cancel: &CancellationToken) -> Result<ChatResponse>;
    async fn chat_stream(&self, req: ChatRequest, cancel: &CancellationToken) -> Result<ChatStream>;
    async fn validate_key(&self) -> Result<()>; }
trait Reranker { async fn rerank(&self, q: &str, docs: &[&str], cancel: &CancellationToken) -> Result<Vec<f32>>; }
```
- `Resilient<P>` wraps every provider: backon retries that honour `Retry-After`, a stream is never retried after its first token, a governor limit per provider, and cancellation.
- A `PrivacyGate` checks `egress_consent` before any remote call and writes `egress_audit`. **No fallback across the privacy boundary.**

### Config: one schema, with a user-facing side (P1)
- `sr-common::config` is one schemars-annotated module:
  - `Settings` is global, in `settings.toml` (atomic write plus `.bak`).
  - `FolderSettings` and `CollectionSettings` live in the catalog.
- Every field has `#[schemars(title, description, extend("x-tier"="basic"|"advanced", "x-section"=…, "x-restart"=bool, "x-requires"=…))]`.
- `xtask` exports the JSON Schema. The UI generates the **Basic** page from `x-tier=basic` and the **Advanced** disclosure from the rest. MCP tool input schemas come from the same module.
- **Validation:**
  - Bounds come from the schema.
  - Cross-field rules live in `Settings::validate() -> Vec<FieldIssue{path, UserMessage}>`, called live by the UI.
  - Loading is lenient per field: a bad value falls back to its default with the notice "Some settings were reset" and the action "Review settings".
  - Unknown keys are kept on rewrite.
- Each setting has exactly one home, so precedence never arises. `policy.toml` (M5) overrides with "Managed by your organization".

| Section | Field (default) | Tier | Validation / notes |
|---|---|---|---|
| General | start_with_windows (true) | Basic | Toggles the HKCU Run entry |
| General | notify_on_problems (true) | Basic | |
| General | language (system) | Basic | |
| Search | search_model (`auto` = the detected tier's model; starts on the bundled bge-small) / `multilingual` / `cloud:<provider>` | Basic | `cloud` requires a provider plus "file text" consent. (2026-11-08: `settings.toml` accepts `cloud:<provider>`, but the engine doesn't offer remote embeddings yet, and Settings shows no cloud choice.) Read-only line under it: **"Recommended for your computer: Better search (208 MB download)"** + [Use it]. Shown only when the recommended tier is above the active one and its model isn't in use; [Use it] = consent to download + background re-embed (§2.8). Never auto-downloads. |
| Advanced: search model | `[search] tier_override` (`auto` default, `basic`/`better`/`best` = Minimum/Standard/High), `use_graphics_card` (bool: `true` = Auto, default; `false` = Off; the words `auto`/`off` are accepted). Typed in `sr_common::config::Search` (CO-1) | Advanced | Override above the recommendation shows "This computer may index slowly with this choice. Search keeps working while it runs." It never blocks. `off` forces the CPU EP for new sessions (no re-embed: §2.8 sticky variant). |
| Answers (visible when a provider exists) | answers_enabled (true once a provider is added) | Basic | Consent asked on first use |
| Answers | deeper_answers / iteration pass (false) | Basic | max_rounds is Advanced |
| AI provider | provider, key (keyring), default_model (auto-picked) | Basic | Key validated live |
| Folder (per folder) | display_name, keep_up_to_date (true → sync_mode=auto), file_types_preset (documents_and_code), include_online_only (false) | Basic | Root checked against the dangerous-root list |
| Connected apps | Connect buttons, snippet | Basic | |
| Advanced: indexing | chunk_preset (balanced), chunk size/overlap (none), max_file_bytes (10 MiB, 1 KiB–200 MiB), exclude_globs (+ defaults), reindex_policy (smart), recursive (true), reconcile cadence (60 min, 5–1440), debounce_ms (500, 100–5000), follow_symlinks (false; never outside the root) | Advanced | |
| Advanced: ranking | rrf_k (60, 1–1000), rerank_enabled (false → model download), rerank_top_n (50), mmr_lambda (off), default_max_results (50, ≤150), min_score (0.1), dense_only mode (false) | Advanced | |
| Advanced: providers | base_url, timeouts, rate limits, proxy override, remote embedding batch size | Advanced | URL must be http(s); loopback HTTP allowed for Ollama |
| Advanced: integrations | http_server_enabled (false), http_port (random, remembered), mcp_add_folder_enabled (false), mcp_sync_folder_enabled (false), collections, per-client scopes | Advanced | |
| Advanced: system | data_dir (move with progress), log_level (info), retention_hours (1), indexing threads (auto) | Advanced | |

`min_score` under RRF: the filter applies to the rerank score when reranking is on, otherwise to dense cosine similarity. Lexical-only hits pass if they are in the BM25 top 10. This keeps Selene's 0.1 meaning.

### Error model: one model, with a user-facing side (P1)
```rust
pub struct Error { code: ErrorCode, ctx: ErrCtx /* provider, path(redacted), folder_id, secs */,
                   retryable: bool, retry_after: Option<Duration>, source: Option<BoxError>, trace_id }
pub struct UserMessage { title: String, body: String, action: SuggestedAction, details: Details /* code, chain, trace_id */ }
pub enum SuggestedAction { Retry, WaitAndRetry(u32), OpenSettings(Section), ReenterKey(Provider), AddProvider,
    AllowRemote(Provider, Purpose), ChooseFolder, ChooseSubfolder(PathBuf), ReviewApproval(Ulid), ViewSkippedFiles(Ulid),
    FreeDiskSpace, RestartApp, RestartClient(ClientKind), ReconnectClient(ClientKind), CopyDiagnostics, None }
```
- `UserMessage::from(&Error)` is the **only** renderer, and its text is localised.
- Tauri gets an `ErrorDto` holding the UserMessage.
- MCP gets a tool result with `isError: true`. Its text is title + body + an agent phrasing of the action (for example "Ask the user to re-enter their OpenAI key in Selene Retrieval"), and `structuredContent` carries `{code, retryable, retry_after}`. Bad arguments use JSON-RPC `-32602`.
- Searches return `Result`, never a silent `[]` (W11). A search with no close match returns an empty list **with `no_good_match: true`** (R-UX18), which MCP renders as "Nothing closely matched “…” in the user's folders."
- MCP error text is "Title. Body Agent-phrase" (the title ends as its own sentence; M2-R).
- Path refusals: `..`, absolute or UNC outside the folders, `\\?\`, drive-relative, junctions out, **and** ADS (`a.txt:s`), device names (`CON`, `nul.txt`) and trailing dot or space → `PathOutsideSandbox`; denylisted secrets → `PathDenied`; a malformed path (empty, `<>|?*`, a component over 255 chars) → `InvalidInput` (M2-R; no new code).

| # | ErrorCode | Title | Body | Action | Retry |
|---|---|---|---|---|---|
| 1 | InvalidInput | That request wasn't valid | Something in the request was missing or out of range. | None (MCP: corrected arguments) | No |
| 2 | NotFound | Not found | That folder or file no longer exists in Selene Retrieval. | Retry | No |
| 3 | PathOutsideSandbox | That file is outside your folders | Selene Retrieval can only open files inside folders you've added. | ChooseFolder | No |
| 4 | PathRefused | This folder can't be added | It contains private system or account settings. Pick a folder inside it instead. | ChooseSubfolder(path) | No |
| 5 | PathDenied | This file is private | Files that usually hold passwords or keys (like .env or .pem) are never read. | None | No |
| 6 | FolderOverlap | Already included | This folder is already part of "{other}". | None | No |
| 7 | FolderUnavailable | Folder not available | "{name}" can't be reached. The drive may be unplugged or the folder moved. | ChooseFolder | Auto when it reappears |
| 8 | PermissionDenied | No permission to read | Windows didn't allow access to "{name}". | Retry | Yes |
| 9 | FileTooLarge | File too large | "{file}" is bigger than the {limit} limit and was skipped. | ViewSkippedFiles | No |
| 10 | FileLocked | File is in use | Another program is using "{file}". It will be retried automatically. | Retry | Yes |
| 11 | FileOnlineOnly | File is online-only | "{file}" is stored only in OneDrive and was skipped. | OpenSettings(Folder) | No |
| 12 | FileUnreadable | Couldn't read this file | "{file}" may be damaged, password-protected or in an unsupported format. | ViewSkippedFiles | No |
| 13 | ExtractTimeout | File took too long | "{file}" took too long to read and was skipped. | ViewSkippedFiles | No |
| 14 | ApprovalRequired | Waiting for your approval | {client} wants to add "{path}". | ReviewApproval(id) | — |
| 15 | ApprovalDenied | Request declined | You declined {client}'s request, or it expired. | None | No |
| 16 | Conflict | Still busy with that folder | This folder is being changed right now. Try again in a moment. | WaitAndRetry(5) | Yes |
| 17 | Busy | Busy indexing | Selene Retrieval is catching up. Search still works; this action will run shortly. | WaitAndRetry(30) | Yes |
| 18 | Cancelled | Stopped | The operation was stopped. | None | No |
| 19 | Timeout | That took too long | The operation didn't finish in time. | Retry | Yes |
| 20 | ProviderNotConfigured | No AI provider set up | Answers and prompt improvement need an AI provider. Search works without one. | AddProvider | No |
| 21 | ProviderAuth | Your {provider} key was rejected | The key may be wrong, expired or revoked. | ReenterKey(provider) | No |
| 22 | ProviderQuota | {provider} account is out of credit | Your {provider} account has no remaining quota. | OpenSettings(Providers) | No |
| 23 | RateLimited | {provider} is limiting requests | Too many requests right now. Retrying in {secs} seconds. | WaitAndRetry(secs) | Yes |
| 24 | ProviderUnavailable | {provider} isn't responding | The service is having problems. | Retry | Yes |
| 25 | ProviderRequest | {provider} couldn't handle this request | The model may not exist anymore or the input was too long. | OpenSettings(Providers) | No |
| 26 | PrivacyBlocked | Not sent to {provider} | You haven't allowed sending {purpose} to {provider}. | AllowRemote(provider, purpose) | No |
| 27 | Offline | You're offline | Selene Retrieval couldn't reach {host}. Local search still works. | Retry | Yes |
| 28 | NetworkBlocked | Connection blocked | A proxy or firewall blocked the connection to {host}. | CopyDiagnostics | No |
| 29 | ModelDownloadFailed | Download didn't finish | The {model} download stopped. It will resume where it left off. | Retry | Yes |
| 30 | ModelIntegrity | Download was damaged | The downloaded {model} didn't pass its safety check and was deleted. | Retry | Yes |
| 31 | DiskFull | Not enough disk space | Indexing paused. About {need} more is needed on {drive}. | FreeDiskSpace | Auto on space |
| 32 | Corrupt | Repairing search index | Part of the index was damaged and is being rebuilt. Results may be incomplete for a while. | None | Auto |
| 33 | ServiceUnavailable | Background service didn't start | Selene Retrieval's background service isn't running. | RestartApp | Yes |
| 34 | UpgradeRequired | Finish updating | Selene Retrieval was updated. Restart {client} to finish. | RestartClient(client) | No |
| 35 | SettingsInvalid | Some settings were reset | A few settings had invalid values and were set back to defaults. | OpenSettings(section) | No |
| 36 | Internal | Something went wrong | An unexpected error happened. | CopyDiagnostics | No |
| 37 | ModelIncompatible | Couldn't switch search model | The new search model didn't work as expected, so your current search was kept. | CopyDiagnostics | No |

GPU problems have **no** user-facing code. A DirectML registration error, canary failure or device removal falls back to the CPU silently. It shows up only as an `ep.changed` event and in diagnostics (§2.8).

### Events (`sr-common::events`): one bus, push-only (P1: progress is always visible, never polled)
- Every event is wrapped as `Seq<T>{seq, ts, payload}`.
- Event types: `FolderStatusChanged`, `ScanProgress{done, total_est}`, `IndexProgress{files_done, files_total, eta_s}`, `FileFailed{code}`, `QueueDepth`, `IndexCommitted{store, seq}`, `ProviderHealth`, `ApprovalRequested`, `PurgeCompleted`, `ClientConnected`, `SettingsChanged`, `HostState{Starting|Ready|Draining}`, `CatchUpProgress{done, total}` (restart catch-up, 4.24), `Notice{notice}` (a plain notice such as "Some settings were reset", R-UX19), plus the tier and model events below (they replace v1's `ModelDownload{bytes, total}`).
- Tier and model events (Amendments 1 and 2). The serde tag is the dotted wire name. Payloads are final for M1/M2:

| Wire name | Variant and payload | Emitted when | UI use |
|---|---|---|---|
| `tier.detected` | `TierDetected{recommended: Tier, active: Tier, override_: Option<Tier>, ep: EpPlan, gpu_class, reasons: Vec<String>}` | Every daemon start (detection ≈ 0.2 s, off the UI thread) and on display-adapter change (`WM_DEVICECHANGE`/DXGI) | "Recommended for your computer" line; reasons only under Details |
| `ep.changed` | `EpChanged{from, to, reason_code}` | GPU probe result, canary failure, `DXGI_ERROR_DEVICE_REMOVED`, user switch | Diagnostics only. Silent in Basic |
| `model.download.progress` | `ModelDownloadProgress{model_id, bytes, total}` | At most every 250 ms during a download | Progress bar in Settings → Search |
| `model.download.completed` | `ModelDownloadCompleted{model_id, sha256}` | After the sha256 check passes and the rename completes | — |
| `model.download.failed` | `ModelDownloadFailed{model_id, code: ModelDownloadFailed\|ModelIntegrity\|Offline\|NetworkBlocked\|DiskFull, resumable}` | Network drop (partial file kept) or sha256 mismatch (partial file deleted) | Mapped UserMessage; search unaffected |
| `reembed.started` | `ReembedStarted{index_id, model_id, dims, total, resumed_from}` | Shadow build starts or resumes | "Improving search… (search keeps working)" |
| `reembed.progress` | `ReembedProgress{index_id, done, total, eta_s}` | After each committed batch, throttled to 4 Hz | Progress plus ETA |
| `reembed.failed` | `ReembedFailed{index_id, code, resumable}` | Interruption (resumable), `ModelIncompatible` or `DiskFull` | Toast only if not resumable |
| `index.swapped` | `IndexSwapped{collection_id, old_index_id, new_index_id, model_id, dims}` | After the atomic pointer swap | "Search upgraded" toast; MCP `resources/updated` for `sr://status` |
- `EngineStatus` is a snapshot that includes a plain `summary`, `catch_up: Option<{done, total}>` (summary "Catching up after restart (n%)"; search serves the last committed state meanwhile) and `notices: [{code, message: UserMessage, fields}]` (kept until the next start).
- Adapters:
  - app-RPC (and from there a Tauri Channel)
  - MCP `notifications/progress` for calls carrying a progress token
  - `resources/updated` for `sr://status` and `sr://folders`

## 2.6 Security by design (on by default, invisible to the user)

Per P1, every control is on by default and needs no user action. Friction is absorbed by defaults and plain messages, never by weakening a control.

| Threat | Control |
|---|---|
| Folder overreach (W13) | Roots are added only through the app UI or the hidden `admin` CLI. `add_folder` is off, and when on needs approval in our own UI. Dangerous roots are always refused, with a ChooseSubfolder action. AccessGrant is separate from Indexed. **Amendment 13:** "Create a folder for me" makes `Documents\Selene Retrieval` in the **app** (as the user, never elevated) and adds it with the same privileged `folders.add`; the UI never sends that path, and the service still never writes into user folders. |
| Path traversal | All I/O goes through `SandboxFs` (a cap-std `Dir` per root). Rejected: `..`, absolute paths, UNC, `\\?\`, ADS `:`, device names by stem, trailing dot or space, and 8.3 names after expansion. Reparse points aren't followed outside the root. Path keys are case-folded and dunce-normalised. `read_file` copies `path-utils.ts` realpath-of-first-existing-ancestor semantics and the 5 MiB `read-utils.ts` limit. |
| Secret leakage | `SecretFilter` default denylist (`.env*`, `*.pem`, `*.key`, `*.pfx`, `*.p12`, `id_rsa*`, `id_ed25519*`, `.ssh/`, `.aws/`, `.gnupg/`, `.npmrc`, `.pypirc`, `credentials*.json`, `*.kdbx`) applied to the walker, watcher, read_file, grep, answer and iterate. |
| Local attack surface | Pipe: user-SID DACL, `PIPE_REJECT_REMOTE_CLIENTS`, `FIRST_PIPE_INSTANCE`. **No TCP by default.** Opt-in HTTP: 127.0.0.1 only, 256-bit token per client (keyring), rmcp `allowed_hosts` loopback, Origin check, tower-http body (4 MiB), timeout and concurrency limits. |
| Parser DoS / RCE | `sr-extract`: low IL, Job Object memory and time limits, no file system access, zip-bomb guards. |
| Prompt injection via indexed content | Answer and iterate loops can only call `read_file` within scope; no write or exec tools; retrieved text is fenced as untrusted data; ≤ 6 tool steps; capped token budget. |
| Silent egress | Consent per (provider, purpose); `local_only` until consent is given; no fallback across the privacy boundary; every call audited (including query embeddings and rerank). |
| Filter injection (W3) | No string-built filters; ULIDs parsed; one fuzz-tested `lance_filter` helper. |
| Secrets at rest | Credential Manager only; redacted in tracing; never in settings.toml. |
| DLL hijack | Static ORT where it links (M0 gate), else `onnxruntime.dll` loaded by absolute path (`ort` `load-dynamic`, `init_from`). `DirectML.dll` ships app-local and resolves only from the app dir or System32: `sr-daemon` calls `SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32 \| LOAD_LIBRARY_SEARCH_APPLICATION_DIR)` before ORT loads. **Every shipped exe hardens its DLL search as the first statement of `main()`** (S-08, M5-I): `sr-daemon` as above; `sr-extract` System32 only (`win::harden_process`); `sr-app`, `sr-bridge`, `sr-launch` and `sr-setup` call the shared `sr_client::harden_dll_search()` (M5-E; System32 + app folder, same flags as the daemon; the WebView2 runtime is loaded by absolute path and imports System32 DLLs only). The current directory and `PATH` are never searched. CI: `cargo xtask check-manifest` (source check over all six mains). The MSI custom-action DLL `sr-msi-ca` runs inside msiexec and must not change that process's search order; it loads nothing by name (static imports of `msi.dll`/`advapi32.dll` only). Both DLLs pass a `WinVerifyTrust` Microsoft-signature check before the first load; on failure, ORT falls back to CPU (static) or embedding is disabled with `Internal`, and nothing is ever loaded by name. `sr-daemon` links `DirectML.dll`, `d3d12.dll` and `dxgi.dll` only as delay-load imports (its own `build.rs` emits `sr_providers::dml::DELAYLOAD_ARGS`), and `harden_dll_search()` is the first statement of `main()` (4.8). CI: `cargo xtask check-delayload`. |
| Reviewed `unsafe` | `unsafe_code = "deny"` workspace-wide. Allowed per module, each call with a SAFETY note, only for Win32 FFI in (complete list, checked by the M2 tester with `grep allow(unsafe_code)`): `sr_client::win` (**+ M4-E, R31:** `pipe_client_pid` = `GetNamedPipeClientProcessId` on the server pipe, and `process_image_path` = `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` + `QueryFullProcessImageNameW(PROCESS_NAME_WIN32)` into a 32 768-unit buffer, handle checked and closed once; reviewed by the M4 tester); `sr_extract::win` (+ `sr_extract::probes`, test-probes feature only); `sr_extract_client::sandbox`; `sr_engine::win`; **`sr_providers::dml::imp`** (WinVerifyTrust, LoadLibraryExW, module queries, M1-D; **+ `file_version`**: `GetFileVersionInfoSizeW`/`GetFileVersionInfoW`/`VerQueryValueW` from System32 `version.dll` resolved at run time, M2-G); **`sr_providers::dxgi::imp`** (DXGI/D3D12 COM enumeration, `SHQueryUserNotificationState`; M2-G); **`sr_providers::sys`** (`system32_proc`: `LoadLibraryExW(LOAD_LIBRARY_SEARCH_SYSTEM32)` + `GetProcAddress`; `free_disk`: `GetDiskFreeSpaceExW`; M2-G); **`sr_providers::download::metered::imp`** (COM `INetworkCostManager` on its own MTA thread; M2-G); **`sr_sync::fsid::imp`** (`GetFileInformationByHandleEx(FileIdInfo)`, W2-S; **+ `GetDriveTypeW`** on a NUL-terminated buffer we own, M2-W). **`sr_providers::remote::proxy::win`** (M3-P; reviewed by the M3 tester): `winhttp.dll` exports `WinHttpGetIEProxyConfigForCurrentUser`, `WinHttpGetDefaultProxyConfiguration`, `WinHttpOpen`, `WinHttpGetProxyForUrl` (WPAD/PAC), `WinHttpSetTimeouts`, `WinHttpCloseHandle` resolved from **System32** at run time via `sr_providers::sys::system32_proc` (never imported by name), returned strings freed once with `GlobalFree`. Note: the 5 s WinHTTP timeouts bound PAC download, not WPAD discovery itself (slow-WPAD risk R32 in 04). Installer helper `sr-setup` (nested workspace `installer/sr-setup`, M2-I): **`proc`** (Toolhelp snapshot, `QueryFullProcessImageNameW`, `TerminateProcess`, `WaitForSingleObject`; current user's processes only), **`taskfolder`** (Task Scheduler COM: delete the empty `\SR` folder), **`clients`** (`SHGetKnownFolderPath(FOLDERID_RoamingAppData)`; **+ `FOLDERID_Profile`** for the Cursor `~\.cursor` root, M4-A; reviewed by the M4 tester). **`creds`** (M3-U; reviewed by the M3 tester): `CredEnumerateW` with the filter `SeleneRetrieval/*`, prefix re-checked, generic type only, target names only (no secret read), `CredDeleteW` per target, one `CredFree`; current user only. sr-setup **`autostart`** (M5-I): `RegDeleteKeyValueW(HKEY_CURRENT_USER, …)` on the Run and StartupApproved\Run keys, our value names only. **`sr-msi-ca`** (nested workspace `installer/msi-ca`, M5-I): `RegGetValueW` (WebView2 `pv`, REG_SZ into an owned buffer), `MsiGetPropertyW`/`MsiSetPropertyW`, `MsiCreateRecord`/`MsiRecordSetStringW`/`MsiProcessMessage`/`MsiCloseHandle` on the handle msiexec passes. **`sr_providers::cli::job::imp`** (M5-C, CLI sign-in providers; **reviewed by the M5 tester**: handle null-checked, owned and closed once also when `SetInformationJobObject` fails, struct size matches the class, the child handle stays open while the tokio `Child` is held, `GetSystemDirectoryW` result rejected when `n >= 512`): `CreateJobObjectW` (no name, default security) + `SetInformationJobObject(JobObjectExtendedLimitInformation, KILL_ON_JOB_CLOSE)` on a zeroed struct, `AssignProcessToJobObject` with the live tokio child's handle, `CloseHandle` once on drop (kills the CLI's process tree); `GetSystemDirectoryW` into a 512-unit buffer we own (absolute `cmd.exe` for the sign-in window). Test-only: `sr-bridge` `tests/pipe_security.rs` (+ `tests/common/mod.rs`), the never-packaged `sr-test-daemon` (`src/test_daemon.rs`: one `std::env::set_var` as the first statement of `main`, M5 tester), `sr-extract` `tests/acl_layout.rs`. Every DLL these load is resolved from System32 or the app dir by absolute path. Any new module needs a review entry here. |
| Model tampering | Every model file's sha256 and HF revision are pinned in the binary (TierProfile). It is checked after download, and the bundled model is checked at load. A mismatch deletes the file (`ModelIntegrity`). |
| Data at rest | Protected DACL on the data dir; `secure_delete=FAST`; purge SLA. |
| Desktop | Minimal Tauri capabilities, strict CSP, no shell plugin; updater checks minisign plus Authenticode on every exe. |
| Supply chain | cargo-deny (licences, advisories, bans, duplicate majors), committed `Cargo.lock`, cargo-cyclonedx SBOM, CI on windows-latest, pinned toolchain. CI binary guards: `cargo xtask check-bridge-deps` (no engine stack, rayon-core or memmap2 in sr-bridge) and `cargo xtask check-crt` (sr-bridge, sr-launch, sr-extract, sr-app import no VC++ runtime DLL; 4.1). |

## 2.7 Efficiency

- **Change detection and embedding reuse.** Tiered change detection. A chunk-level text_hash diff plus Lance text_hash lookup means unchanged and duplicate text is never re-embedded.
- **EmbedBatcher.** One queue per profile, flushed at the token budget or after 50 ms. Concurrency is capped per provider. Local inference runs on a dedicated rayon pool (cores − 2).
- **Index writes.** A single IndexWriter with group commits closed at 2 s, 512 embedded chunks or 1 000 outbox ops, whichever comes first (Amendment 4.25). The pipe opens before outbox replay (4.24). Query embedding always runs on the CPU (4.27). Lance `optimize` above a fragment threshold. The ANN index is built above 50k rows and retrained when the row count doubles. The tantivy writer heap is 128 MB.
- **Resumable.** The durable job queue uses epoch leases, plus outbox replay.
- **Fair and backpressured.** Deficit round-robin across folders, so one huge folder can't starve the others. Backpressure comes from claim permits.
- **Fast first results.** The first index of a folder orders jobs newest-first.
- **Read path.** WAL read pool, moka caches (query embedding LRU of 1k, file tree with 30 s TTL), batched hydration.
- **Windows specifics.**
  - Skip online-only files.
  - Retry `ERROR_SHARING_VIOLATION`.
  - SMB roots use a scheduled reconcile instead of a watcher.
  - Watcher overflow triggers a rescan.
- *Throughput:* measured in M0-E (`research/m0-e-gpu-tier.md`) at a worst case of 512 tokens per chunk. The v1 estimate (200–500 chunks/s on 8 CPU cores) was far too optimistic for the CPU. It holds for DirectML on a dedicated GPU. All numbers scale with the tier (§2.8).

## 2.8 Hardware tiers, GPU and re-embed (Amendments 1 and 2)

Evidence and all numeric values: `research/m0-e-gpu-tier.md` (authoritative tier table). Spike code: `spikes/gpu-tier/src/{hw,tier,profiles,reembed}.rs`.

### Contracts (`sr-common::tier`, M1)
```rust
pub struct HwInfo { total_ram: u64 /* rounded to GiB */, physical_cores: u32, logical_cores: u32, avx2: bool,
                    avx512_vnni: bool, free_disk: u64 /* data-dir volume */, gpus: Vec<GpuInfo> }
pub struct GpuInfo { name, vendor_id, device_id, dedicated_vram: u64, shared_mem: u64, is_software: bool,
                     d3d12_fl11: bool, uma: bool, dxgi_index: u32 }
pub enum Tier { Minimum, Standard, High }                       // UI: Basic / Better / Best
pub enum GpuClass { None, Integrated /* UMA or < dgpu_min_vram */, Dedicated }
pub enum EpPlan { Cpu, DirectMl { dxgi_index: u32 }, ProbeDirectMl { dxgi_index: u32 } }
pub struct Thresholds { dgpu_min_vram: 4 GiB, high_min_vram: 6 GiB, std_min_ram: 8 GiB, std_min_cores_cpu_only: 6,
                        std_min_free_disk: 10 GiB, high_min_ram: 16 GiB, high_min_free_disk: 25 GiB, probe_min_speedup: 1.2 }
pub fn select_tier(&HwInfo, &Thresholds) -> TierDecision { tier, ep: EpPlan, gpu_class, reasons: Vec<String> }  // pure
pub fn effective_tier(&TierDecision, manual: Option<Tier>) -> (Tier, Option<Warning>)

pub struct TierProfile {                                        // one const per tier, compiled in
    tier, model_id, dims, pooling, query_prefix, max_seq_tokens,
    gpu_file: ModelFile, cpu_file: ModelFile,                   // {hf_repo, hf_revision, path, bytes, sha256}
    batch_gpu, batch_cpu, concurrency_gpu, concurrency_cpu,     // concurrency = batches in flight
    ann: AnnParams { min_rows, partitions_per_sqrt_rows, hnsw_m, hnsw_ef_construction, nprobes, refine_factor },
    rerank: bool, rerank_top_n,
}
pub const CHUNK_TOKENS: u32; pub const CHUNK_OVERLAP: u32;      // NOT per tier: chunk IDs must not change
```
- `select_tier` is pure. All boundaries are `≥` and unit-tested: RAM, cores, disk and VRAM below/at/above; no GPU; iGPU (UMA); an APU reporting a large carve-out; WARP; no FL 11.0; hybrid laptops; no AVX2; manual override.
- Detection runs at every daemon start (≈ 0.2 s, off the UI thread) and on adapter changes. Its result is stored in `meta.hw_detection` and emitted as `tier.detected`. **A changed recommendation never triggers a re-embed by itself.** It only updates the "Recommended" line.
- The TierProfile in force is the **active index's** profile, not the recommended one. Batch size, ANN parameters and rerank follow the index being written or queried.

### Execution-provider selection (M2; M1 uses `Cpu`)
```
EpPlan::DirectMl      → session(DML, error_on_failure) → commit → canary(2 chunks, finite) ─┐ ok → DML
EpPlan::ProbeDirectMl → as above + time 2 batches on DML and CPU; DML iff ≥ 1.2× and cos(DML,CPU) ≥ 0.999 ─┤ (cached per driver version)
any failure / "Use graphics card: Off" / EpPlan::Cpu ───────────────────────────────────────────┴→ CPU session (silent; ep.changed)
during run: DXGI_ERROR_DEVICE_REMOVED → rebuild CPU session, re-run batch; 3 faults/24 h → CPU until driver version changes
DML session options: memory pattern off, sequential execution (DML EP requirements); CPU: intra-op threads = max(1, physical − 2)
batch: start at TierProfile.batch_gpu; halve while a batch takes > 500 ms (desktop responsiveness, TDR margin)
pause GPU work while SHQueryUserNotificationState = QUNS_RUNNING_D3D_FULL_SCREEN | QUNS_BUSY (game or presentation)
```
**Sticky variant.** An index is built with exactly one model file, `gpu_file` (fp16) or `cpu_file` (int8) depending on the EP *when the build starts*, and records its sha256. Later incremental embeds and every query use that same file.
- `ep_for_file`: **fp16/fp32 → current EP (portable).** CPU vs DirectML cosine ≥ 0.99998 for bge-small, arctic-m, arctic-l and gte-large. If the GPU goes away, the same fp16 file runs on the CPU.
- **int8 → always CPU.** DirectML int8 vs CPU int8 gives cosine 0.991–0.997, below the 0.999 bar. If a GPU appears later, the int8 index stays on the CPU, and a query embed on the CPU costs only milliseconds.
- Embedding spaces are never mixed, and an EP change never forces a re-embed.
- Never rely on ORT's implicit EP fallback. With an fp16 model it produced a session that failed to initialise. Always register with `error_on_failure` and build the CPU session explicitly.

### Model download (M2)
- Start condition: the user clicked [Use it] (or chose a tier in Advanced), the machine is not on a metered connection or the user chose "Download anyway", and the disk pre-flight passes.
- Disk pre-flight: free space must be at least `model.bytes + rows × dims × 4 × 1.3 + 2 GiB`, otherwise `DiskFull`.
- Transfer: reqwest GET to `https://huggingface.co/<repo>/resolve/<pinned revision>/<path>`, written to `models/<sha256>.part` with HTTP Range resume. The `model_files` table records `bytes_have`.
- Events: `model.download.progress` at most every 250 ms. On completion, the sha256 is checked against the value pinned in the TierProfile, then the file is renamed to `<sha256>.onnx` and `model.download.completed` is emitted.
- Failure: a network error keeps the `.part` (`ModelDownloadFailed`, resumable, auto-retried with backoff). A sha256 mismatch deletes it (`ModelIntegrity`). **The active index and search are never touched.**

### Re-embed and atomic swap (M2; schema in M1)
```
trigger: user opts in to a tier/model ─► ensure model file (above) ─► load embedder; assert embedder.dims == profile.dims
   │                                        (else ModelIncompatible, nothing written)
   ▼
vector_indexes row (state=building, start_seq = current outbox seq) + new Lance table "vec_<index_id>"
   ▼  job kind=rebuild_profile, priority 3 (below live edits), epoch-leased like every job
bulk pass: chunks in chunk_id order after checkpoint_after_chunk_id
   ├─ text from chunk_text (catalog) → embed batch (TierProfile batch for the EP) → assert len == dims (else discard shadow,
   │  ModelIncompatible) → Lance merge_insert(chunk_id) → commit → UPDATE vector_indexes SET checkpoint=last chunk_id,
   │  rows_done=… (data first, checkpoint second: a crash in between re-upserts the same chunk_ids, which is idempotent)
   ├─ reembed.progress (4 Hz) ; kill/restart/sleep → job re-leased → reembed.started{resumed_from}
   └─ live edits meanwhile go to the ACTIVE index as usual (outbox), and are recorded in index_ops
catch-up: replay index_ops with seq > start_seq into the shadow (coalesced per file_id, same code as startup replay)
validate: shadow chunk_id set == catalog chunk set for the collection; dims; ANN index built if rows ≥ ann.min_rows
swap (one SQLite transaction): collections.active_index_id = new; new.state=active; old.state=retired → index.swapped
   queries snapshot active_index_id at start; in-flight queries finish on the old table
GC: retired tables dropped after 15 min with no readers (retention job; part of the purge SLA)
```
Every step above is covered by the spike simulation (`spikes/gpu-tier/src/reembed.rs`, 10 tests): happy path with chunk IDs unchanged; resume from the checkpoint (only the remaining chunks are embedded); a crash between data and checkpoint (no duplicates); dims declared wrong (rejected before any write); dims returned wrong (shadow discarded); a query with the wrong model (refused, not compared); a download network drop (search on the old index, then Range resume); a corrupt download (deleted, active kept); edits during the re-embed (caught up before the swap); and a crash before the pointer swap (old index kept, then completed).
