# Selene Retrieval: Plan v1 overview, review resolution and decisions

> Status: approved and implemented (v0.1.0). This is the design record; later decisions are the Amendments at the end.
> Binding: Product Principles P1 (average-user simplicity, never weakening security) and P2 (standing check), see `product-principles.md`.

Inputs: the draft v0 (`_draft-plan-v0.md`, superseded), the architect's review (`research/architect-review-v1.md`), `research/phase1-findings.md`, P1/P2, spot-checks of the Selene source, and a crates.io check of items v0 marked unverified (2026-10-17).

## Changes vs v0

1. **Topology.** The architect's D1 is accepted with one change: `sr-daemon` is the only engine process. `sr-app` and `sr-bridge` are clients over a user-only named pipe. Host handoff is gone.
   - The daemon is launched through a per-user on-demand **scheduled task**. This is the main path, not a fallback.
   - Reason: Claude Desktop on Windows ships as MSIX and its file system is virtualised. A daemon spawned directly by its child process would inherit that package context and could write its data into Claude's package folder. Job Object breakaway would not fix that.
2. **The bridge is a byte proxy.** It sends one handshake frame, then copies raw MCP bytes. The MCP session lives in the daemon (rmcp `transport-async-rw` over the pipe). Version skew between bridge and daemon almost disappears.
3. **No TCP port by default.** Streamable HTTP is opt-in under Advanced. The stdio snippet carries no token.
4. **Identity, outbox and text.** D2, D3 and C3 are adopted: chunk IDs include `file_id`, there is an outbox with per-store watermarks, and chunk text is kept in the catalog (zstd, refcounted).
5. **Jobs.** D4 is adopted, simplified to epoch fencing with no heartbeat. Folder status gains `synced_with_errors` plus a dead-letter list.
6. **Crates.** The five crates you named are kept. Six justified extras are added: `common`, `catalog`, `engine`, `client`, `bridge`, `extract`. The bridge, app and extract worker never link LanceDB or ort.
7. **Extraction isolation.** PDF, DOCX, XLSX and HTML parsing runs in an isolated `sr-extract` worker: Job Object, low integrity, no file system access, bytes in over a pipe. Plain text and code stay in-process.
8. **ANN in M1.** M1's latency and recall targets are now consistent with each other (C5).
9. **A minimal first-run app and signed installer move to M2.** M2 is the first public beta. M1 is an internal dev preview, so R-UX1 never reaches users.
10. **figment is dropped** (last release May 2024, and P1 removes the need for four config layers). Config is serde + `toml` 1.1.8 with a single app-written `settings.toml`. An enterprise `policy.toml` comes in M5.
11. **DirectML is dropped from v1.** It is in maintenance mode and needs DLL loading (M8). That closes R-UX7. *(Partially reversed by **Amendment 2**: the GPU is used by default.)*
12. **Default model.** bge-small-en-v1.5 (int8, 384 dims) ships inside the installer. *(Amended by **Amendment 1**: it is now the Minimum tier; Standard and High download on demand.)* ONNX Runtime is linked statically, so there is no `onnxruntime.dll` to hijack (to verify in M0). Multilingual search is a one-click download.
13. **Version fixes.**
    - The fastembed feature names were wrong. The correct ones are `ort-download-binaries-rustls-tls` and `hf-hub-rustls-tls`; we use neither hf-hub feature.
    - `rusqlite_migration` 2.6.0 needs **MSRV 1.95**, so `rust-version` moves from 1.88 to 1.95.
    - Added: ulid 3.0.0, moka 0.12.16, zstd 0.14.3, toml 1.1.8, zip 9.0.4, quick-xml 0.42.0, html2text 0.17.3, grep-searcher 0.1.17, windows-sys 0.61.2, and the Tauri plugins.
14. **C runtime.** A new decision row. Shipping requires no VC++ redistributable: either `+crt-static`, or app-local `vcruntime140*.dll`. Decided by the M0 gate.
15. **User-facing errors and config.** There is a full ErrorCode → UserMessage table (37 codes; #37 `ModelIncompatible` added by Amendment 1) and a Basic/Advanced settings schema (`02-architecture.md` §2.5).
16. **Tools hidden until usable.** MCP tools that need a provider (`answer`, `enhance_prompt`) are hidden until one is configured, and announced with `tools/list_changed`.
17. **Privacy and disk.** There is a purge SLA, `secure_delete`, one tantivy index per collection (fixes IDF skew, makes purge a directory delete), and a consent model for every egress purpose.
18. **Faster first results.** The first index of a folder processes the most recently modified files first, so search is useful within minutes.

---

## 0. Review resolution

### 0.A Architect issues

| ID | Decision | Resolution and reason |
|---|---|---|
| **C1** chunk ID collisions | **Accept (D2)** | `file_id` is a ULID that stays stable across renames, matched by (volume_serial, 128-bit FILE_ID_INFO). `chunk_cfg_fp = blake3(chunker_version‖splitter params‖tokenizer id‖extractor_version)`. `chunk_id = blake3(file_id‖content_hash‖chunk_cfg_fp‖ord)`. `text_hash = blake3(NFC text)` is used only for dedupe. The derived stores hold no paths. |
| **C2** generation manifest | **Accept D3, with one change** | Writing Lance table metadata *in the same commit* as `merge_insert` is unverified, so the design doesn't depend on it. The rule is **at-least-once plus idempotent**: each store's watermark is persisted after its data commit (tantivy commit payload, which is atomic; Lance metadata in a follow-up commit). On restart, ops from `min(watermark)` are replayed. Replay is safe because deletes are by file_id plus upserts by chunk_id, coalesced per file_id. Queries drop hits whose chunk_id is not in the file's current version. |
| **C3** not rebuildable | **Accept** | Add `chunk_text(text_hash PK, zstd, refcount)`. tantivy indexes the body but doesn't store it. Lance holds vectors plus filter columns. Snippets are hydrated from the catalog. A rebuild re-embeds (CPU time, shown as "Improving search…"). |
| **C4** host races | **Accept D1, with changes** | See `02-architecture.md` §2.2. (1) The spawn race is closed with `LockFileEx(spawn.lock)`, then the pipe is re-checked. (2) There is no handoff. (3) A per-user `host.lock` plus a pipe name keyed on the user's SID fixes the RDP/console double host. (4) Job Object kill and MSIX package context are both avoided by launching through the scheduled task. (5) rmcp lives in the `sr-mcp-server` library inside the daemon. (6) Skew: the bridge is a byte proxy, and the app protocol accepts N-1. |
| **C5** latency contradiction | **Accept: ANN moves into M1** | IVF_HNSW_SQ (preferred, flagged) or IVF-PQ above 50k rows per profile, with refine 5–10. A flat filtered scan is used when the filter selects under 5% of rows. M1 exit: p95 under 200 ms at 1M chunks (384 dims, CPU, no rerank) **and** recall@10 ≥ 0.95 against the flat scan. |
| **M1** in-process extraction | **Accept, modified** | `sr-extract.exe` runs pdf, docx, xlsx and html. The daemon reads the bytes through SandboxFs and sends them over an anonymous pipe, so the worker needs **no file system access**. Limits: low-integrity token, Job Object (1 GiB, kill-on-close, active-process 1, UI restrictions), worker pool of `min(4, cores/2)`, 60 s wall time per job, after which the worker is killed and restarted and the file is recorded as `ExtractTimeout`. Text, markdown and code stay in-process. `panic = "unwind"` is used workspace-wide; Cargo can't set panic per package, and unwind allows `catch_unwind` per job. |
| **M2** embedding dedupe | **Accept** | Look up `text_hash IN (…)` (≤ 500 per query) in the profile's Lance table using a BTREE index, and copy vectors that are found. No separate cache. GC happens with the chunk rows. |
| **M3** single connection | **Accept** | One writer thread plus 3 read-only connections. Pragmas: WAL, `synchronous=NORMAL`, `foreign_keys=ON`, `busy_timeout=5000`, `wal_autocheckpoint=1000`, `secure_delete=FAST`, `auto_vacuum=INCREMENTAL`. Run `PRAGMA optimize` on close and every 6 h. |
| **M4** thin jobs schema | **Accept D4, simplified** | No heartbeat. `lease_owner = daemon_epoch`, incremented at each start; at startup, every `leased` row with an older epoch goes back to `queued`. Claim loop: deficit round-robin across folders, priority 0–3, aging, and permits that depend on downstream capacity. That permit check is where backpressure lives. Done rows move to `job_history` (kept 7 days). |
| **M5** folder status | **Accept** | Statuses: `pending | scanning | indexing | synced | synced_with_errors | partial | paused | unavailable | error`. `dead` files are excluded from the derivation and listed as "couldn't be read". |
| **M6** crate boundaries | **Accept the principle, reconcile names** | See `02-architecture.md` §2.1. The bridge depends only on `sr-common` and `sr-client`. |
| **M7** cross-process push | **Accept** | Every `EngineEvent` carries `seq: u64`, and snapshots carry the `seq` they reflect. The daemon keeps a 4096-event ring. `subscribe(from_seq)` either replays events or returns `Resnapshot`. |
| **M8** onnxruntime.dll hijack | **Accept** | Link ORT statically. If a dynamic library is needed, load it by **absolute path**, never by name. pdfium is loaded by absolute path inside `sr-extract`. Every binary calls `SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32)`. |
| **M9** disk growth and privacy | **Accept** | Removing a folder: (1) hidden from queries immediately; (2) chunk_text refcounts decremented and rows deleted in the same transaction; (3) Lance delete, `optimize`, `cleanup_old_versions(0)`; (4) tantivy merge and GC, or a directory delete if the collection is gone. Nightly retention keeps at most 1 h of Lance versions. SLA: Q7. |
| Minor: racy-git | Accept | Re-hash a file when `mtime ≥ scan_start − 2 s`. |
| Minor: file IDs | Accept | `FileIdInfo` gives a 128-bit ID, plus volume_serial. |
| Minor: overlapping folders | Accept, with a P1 message | Within one collection, an overlap is refused with "Already included in 'Projects'". Across collections it is allowed; dedupe happens through text_hash. |
| Minor: denylist robustness | Accept | Expand 8.3 names (`GetLongPathNameW`), reject trailing dots and spaces, match device names by stem. |
| Minor: denylist everywhere | Accept | One `SecretFilter` applied to the walker, watcher, `read_file`, `grep`, answer and `iterate`. |
| Minor: re-validate roots at startup | Accept | A failure sets the folder `unavailable` (not deleted) with a plain message. |
| Minor: model by sha256 plus consent | Accept | The bundled model is verified at load. Downloads need a click, and are resumable and sha256-checked. |
| Minor: audit query embeddings and rerank | Accept | Purposes: `embed_doc, embed_query, rerank, answer, enhance, iterate, validate_key, list_models`. |
| Minor: no text logging at INFO | Accept | A `Redacted<T>` wrapper; query and chunk text only at TRACE in debug builds. |
| Minor: config vs catalog precedence | Accept, by construction | Each setting has exactly one home: global settings in `settings.toml`, per-folder and per-collection settings in the catalog. `policy.toml` (M5) overrides both and shows "Managed by your organization". |
| Minor: IDF skew | **Resolve** | One tantivy index per collection. Search across collections fuses per-index BM25 ranks through RRF. |
| Minor: Selene mode semantics | Accept | The M6 importer resolves `indexing_mode=auto` from Selene's global flag at import time and writes the mode explicitly. Switching to files_only deletes vectors. (2026-11-08: the files_only mode is removed, Amendment 17.) |

### 0.A Open questions: recommended answers

1. **Is D1 accepted?** Yes, with the C4 changes: scheduled-task launch, byte-proxy bridge, and HTTP off by default.
2. **Default embedding profile?** bge-small-en-v1.5, int8 ONNX, 384 dims, CLS pooling, query prefix "Represent this sentence for searching relevant passages: ", bundled in the installer.
   - The app offers a one-click switch to multilingual-e5-small (384 dims), downloaded with consent, and suggests it automatically when the Windows display language isn't English.
   - jina-v2-base-code and remote profiles are under Advanced.
   - *Unverified:* the exact file size (est. 35–70 MB) and whether fastembed's user-defined model path supports CLS pooling with this prefix (M0).
3. **Per-client collection scopes?** Yes, default "All my folders". For pipe clients the scope is a privacy and usability feature, not a security boundary, because the same user can read the files anyway. For HTTP clients it is bound to the token and enforced. Scopes can be edited in Advanced → Connected apps (M4).
4. **Answer and iterate scope?** The queried scope only: the Indexed folders of the requested collections, intersected with the client's scope. `read_file` in the answer loop is limited to result files or files under those folders, and passes through SecretFilter. AccessGrant roots are excluded unless named explicitly.
5. **How is headless `add_folder` approved?**
   - The tool is off by default.
   - A call creates an `approvals` row and returns `pending_approval` with text the agent can relay: "Waiting for you to approve this in Selene Retrieval."
   - The daemon emits `ApprovalRequested`. A running app shows a notification. If the app isn't running, the daemon launches `sr-app --approve <id>`.
   - Requests expire after 10 min. There is never auto-approval.
   - MCP elicitation is rejected, because the requesting client's UI isn't trusted for this decision.
6. **Data at rest: encryption or ACLs?** ACLs suffice for v1: a protected DACL (user plus SYSTEM, no inheritance) on `%LOCALAPPDATA%\SeleneRetrieval`. BitLocker is the right layer for disk encryption. Secrets live only in Credential Manager.
7. **Purge SLA?**
   - Hidden from search immediately.
   - Catalog text deleted immediately.
   - Physical removal from Lance and tantivy within 15 min while running, or within 15 min of the next start. A `PurgeCompleted` event updates the UI from "Removing…" to "Removed".
   - "Delete all my data" deletes the data directory right away.

### 0.B P1/P2 review

#### (a) Elements that fail "Can an average user do this without documentation?"

| # | Element (source) | Why it fails | Fix (secure default kept) | Lands in |
|---|---|---|---|---|
| U1 | M1 CLI and agent-only folder admin (R-UX1) | Terminal required | M1 is an **internal dev preview**. The first public build (M2) has the wizard. The CLI becomes the hidden `sr-daemon admin …` for support and IT. | M1/M2 |
| U2 | `sr-mcp setup` printing a snippet | Terminal plus JSON editing | Deleted. The app's "Connect" button writes the client config; "Other app" shows a copy-paste snippet. | M2 |
| U3 | Four-layer config | Users can't reason about it; env vars silently override the UI | One app-written `settings.toml`. Env vars only with `--dev`. Bad file → backup, reset, "Some settings were reset" + "Review settings". | M1 |
| U4 | Bridge path, args, token | Users must find an exe path and copy a token | No token over the pipe. Stable path `%LOCALAPPDATA%\Programs\Selene Retrieval\sr-bridge.exe --client <id>`, written by Connect. | M2 |
| U5 | **Claude Desktop MSIX config virtualisation** (verified) | Writing `%APPDATA%\Claude\…` has no effect for MSIX installs | Detect `%LOCALAPPDATA%\Packages\Claude_*` and write to `LocalCache\Roaming\Claude\claude_desktop_config.json`. Merge, back up, verify. Watch the file and show "Disconnected from Claude. [Reconnect]" if it is wiped. | M2 |
| U6 | Daemon state surfacing | Jargon; no user-facing fix | The UI never says "daemon". Tray: "Up to date" / "Indexing 340 of 2,100 files" / "Paused". Silent restart attempts (3× over 10 s), then `ServiceUnavailable` + [Restart Selene Retrieval]. | M2 |
| U7 | Scheduled-task fallback | A user might be asked to create a task | The installer creates `\SeleneRetrieval\Service` silently (per-user, no admin). The uninstaller removes it. The bridge and app recreate it if it is missing. | M2 |
| U8 | Job Object breakaway failure | Could show up as "server disconnected" | Order: task, then breakaway, then in-job (works for that session; replay later). In package context with no task: "Open Selene Retrieval once to finish setup." | M1/M2 |
| U9 | Model download on first run | Fails offline or behind a proxy | Default model bundled. Optional models: one click, size shown, progress, resume, sha256. WinHTTP system proxy and the Windows root store. | M1/M3 |
| U10 | DirectML opt-in | Unfindable; DLL risk | **Amendment 2:** no opt-in at all. The GPU is used by default when it helps, with a silent CPU fallback. Advanced has only an off switch. DLL risk handled by absolute-path loads plus a signature check. | M2 |
| U11 | Folder refusals with a hidden "force" | Dead end or hidden power switch | Always refused: system drive root, `%USERPROFILE%` itself, Windows, Program Files, AppData, our data dir, `.ssh`/`.aws`/`.gnupg`. Action [Choose a subfolder] opens the picker inside the refused folder. A non-system drive root is allowed after a plain confirm. No force switch. | M2 |
| U12 | Very large folders | Hours of CPU with no idea of progress | A pre-scan ETA ("About 40 minutes"), newest files first, search works while indexing. "Choose a smaller folder" only above 500k files. | M2 |
| U13 | Headless `add_folder` approval | Agent waits indefinitely | Q5 flow. | M2 |
| U14 | Version skew after an update | "Refused: proto mismatch" | Byte-proxy bridge, N-1 protocol, "Selene Retrieval was updated. Restart Claude to finish." The updater drains the daemon first. A locked bridge exe is renamed aside (verify in M2). | M2/M5 |
| U15 | Collections vs folders | Two concepts for one job | Basic UI = folders only (default collection "My folders"). Collections and scopes are under Advanced. | M4 |
| U16 | `privacy=local_only` enum | Jargon | Plain consent per purpose, asked the first time it's needed. Remote embeddings get stronger wording ("the text of your files will be sent"). | M3 |
| U17 | Tools that need a provider | The agent calls them and gets an error | Hidden until a provider exists, then announced with `tools/list_changed`. Fallback text: "Ask the user to add one in Selene Retrieval." | M3 |
| U18 | HTTP ports and tokens | Collisions, firewall prompts, token handling | Off by default. When enabled: loopback, remembered random port, snippet pre-filled with a per-client token. | M4 |
| U19 | OneDrive online-only files skipped silently | "Why isn't my file found?" | Folder note plus the toggle "Include online-only files (downloads them)", off by default. | M5 |
| U20 | AV locking | Users told to add AV exclusions | Never ask. Retry with backoff; `FileLocked` + Retry. | M1 |
| U21 | Log files for support | Hard to find | "Copy diagnostics": a redacted bundle copied to the clipboard or saved as a zip. | M2 |
| U22 | Uninstall leftovers | Credentials, task, Run key and client configs remain | The uninstaller cleans all of these and restores client configs. "Also delete my search index" is unchecked by default. | M2/M5 |
| U23 | SmartScreen on early downloads | Scary warning | Artifact Signing from M2; screenshot guidance on the download page. Stays a risk. | M2 |
| U24 | "Start with Windows" | Background sync silently stops if it's off | On by default (wizard checkbox). When off, the daemon exits 10 min after the last client disconnects and the queue is empty. | M2 |
| U25 | Raw `get_status` JSON | Not readable | A plain one-line `summary` field; the app is the status UI from M2. | M2 |
| U26 | Iteration pass toggle | Cost unclear | "Deeper answers (uses more AI credits, slower)", shown only once a provider exists, off by default. | M3 |

#### (b) Setup burden: confirmed or corrected

| Library | Verdict |
|---|---|
| lancedb | **Confirmed.** No user burden. Adds 30–60 MB to the installer (est., measured in M0). lancedb#1051 is an M0 test. |
| tantivy | **Confirmed.** AV locking handled invisibly (U20). |
| fastembed / ort | **Corrected, then amended (Amendment 2).** ORT is linked statically where possible; `DirectML.dll` (and `onnxruntime.dll` if static linking fails) ship app-local, so the user still installs nothing. The GPU is on by default. hf-hub features dropped; our own downloader handles optional models. CI caches the ORT build download. |
| C runtime (new) | **New burden, fixed.** `+crt-static` preferred; app-local VC++ DLLs otherwise. Users install nothing. M0 gate. |
| pdfium-render | **Confirmed, refined.** pdfium.dll ships beside `sr-extract.exe` only and is loaded by absolute path. |
| tokenizers, tree-sitter | Confirmed. A C/C++ toolchain for dev only. `esaxx_fast` dropped. |
| text-splitter, ignore, globset, blake3, notify | Confirmed. |
| rmcp | **Corrected.** No token for stdio (pipe DACL); tokens only for opt-in HTTP. |
| tokio / axum / tower-http | **Corrected.** No port at all by default. |
| rusqlite (bundled) | Confirmed. FTS5 checked in M0. |
| config | **Corrected.** figment dropped; a single file. |
| keyring | **Confirmed.** Uninstall cleanup. Offline key validation → `Offline` + Retry, or saved unvalidated with a notice. |
| reqwest (rustls) | **New.** Platform verifier plus WinHTTP system proxy. *Whether reqwest reads the Windows proxy natively is unverified* (M3). |
| Tauri / WebView2 | **Refined.** The bootstrapper needs internet at install time; an offline enterprise variant comes in M5 (about 127 MB larger). |
| NSIS per-user | Confirmed. Also creates the scheduled task. |
| Signing | **Corrected.** Reduces SmartScreen warnings but doesn't prevent them before reputation builds (U23). |
| autostart / tray | Confirmed. `sr-app --background` creates no webview until a window opens. |

#### (c) User-facing config and error model
See `02-architecture.md` §2.5: Basic/Advanced generation, defaults, validation, and the 37-row ErrorCode table.

#### (d) Zero-config first run, and earlier first-run UX
Per-milestone checks are in `04-roadmap.md`. **Decision: the minimal first-run app (wizard, tray, Connect) and the signed installer move from M4 to M2, which becomes the first public build.** Reasons:
- Under D1 the app is a thin pipe client, so it doesn't block engine work.
- The riskiest P1 items only appear on real machines: Claude's MSIX path, task launch, SmartScreen, Connect. Testing them in M2 halves the risk window.
- The daemon protocol gets a real consumer before it hardens.
- M1 is already heavy.
- R-UX1 never reaches the public.

---

## Decisions needing user approval

1. **D1 topology:** daemon-only engine, app as a client, byte-proxy bridge, launched through an installer-created per-user scheduled task.
2. **Crate set:** your five crates plus `common`, `catalog`, `engine`, `client`, `bridge`, `extract`.
3. **Default model:** bge-small-en-v1.5 int8 (384 dims), bundled, with a one-click multilingual-e5-small switch. → **Amended (Amendment 1, approved):** three hardware tiers.
4. **Moving the minimal app and signed installer to M2** (first public beta; M1 internal). Artifact Signing ($9.99/month) from M2.
5. **Dropping DirectML** from v1 and **dropping figment** for one app-written settings file. → **DirectML part reversed (Amendment 2, approved):** GPU on by default. Dropping figment stands.
6. **No TCP port by default;** streamable HTTP opt-in under Advanced.
7. **Answer and iterate scope** = the queried collections only; **per-client scopes** default "All my folders".
8. **Headless `add_folder` approval** through our own UI (10-min expiry); MCP elicitation rejected.
9. **No app-level encryption at rest** (protected DACL plus BitLocker guidance); **purge SLA** = hidden immediately, physically removed within 15 min.
10. **One tantivy index per collection.**
11. **RRF k = 60** (Selene used 50 and 30), confirmed by the eval harness.
12. **Standalone default file types** = documents and code (the importer keeps Selene's md/txt/pdf/html).
13. **`rust-version` 1.95**; toolchain pinned to the current stable (verify in M0).
14. **Project licence** (placeholder Apache-2.0).

## Amendments (approved by the user, 2026-10-17)

Evidence for both amendments: `research/m0-e-gpu-tier.md` (live model verification, CPU and DirectML benchmarks, tier unit tests, re-embed simulation). Code: `spikes/gpu-tier/`. The tier table and every TierProfile value in that file are authoritative.

### Amendment 1: Tiered embedding models (amends decision 3)

**Decision.** At first run the daemon silently detects RAM, physical cores, AVX2, free disk on the data volume, and GPUs. GPU detection uses DXGI + D3D12, with UMA telling an integrated GPU from a dedicated one; it takes about 0.2 s and needs no admin. The daemon then picks one of three tiers. Everything that scales with the machine sits in one `TierProfile`: model and variant, batch size, concurrency, ANN parameters, reranker use and execution provider.

| Tier (user label) | Model (dims) | Licence | Qualifies when (all `≥`) | Delivery |
|---|---|---|---|---|
| **Minimum** ("Basic", built in) | BAAI/bge-small-en-v1.5 (384): int8 for CPU (32 MB) + fp16 for GPU (64 MB) | MIT | Always. This is the offline zero-config fallback. | **Bundled** (both files, 96 MB); sha256 checked at load |
| **Standard** ("Better") | Snowflake/snowflake-arctic-embed-m-v1.5 (768), fp16 on GPU / int8 on CPU | Apache-2.0 | AVX2, RAM 8 GiB, free disk 10 GiB, and a dedicated GPU of 4 GiB **or** 6 physical cores | On demand (208 MB GPU / 105 MB CPU) |
| **High** ("Best") | Snowflake/snowflake-arctic-embed-l (1024), fp16 | Apache-2.0 | AVX2, RAM 16 GiB, free disk 25 GiB, **dedicated GPU of 6 GiB** | On demand (638 MB) |

> **Superseded in part by Amendment 3.4:** Minimum now bundles bge-small **fp32 only** (≈127 MB, CPU and GPU); int8 variants are off for every tier unless the M1 eval harness clears them.

Rules:
- **Only Minimum is bundled.** Standard and High download only when the machine qualifies **and** the user opts in.
  - Basic settings show one line: "Recommended for your computer: Better search (208 MB download)" [Use it].
  - Advanced has a manual tier override, which warns but never blocks.
- **Downloads** are resumable (HTTP Range) and sha256-checked against values pinned in the binary. They pause on metered connections. A failed or corrupt download never affects search.
- **Every index records its model id, revision, variant sha256 and dims** (`vector_indexes`, `embedding_profiles`).
  - A tier or model change starts a resumable background re-embed into a shadow index.
  - Search keeps using the old index, with queries embedded by the old index's model, until an atomic `active_index_id` swap.
  - A dimension mismatch is rejected and the shadow is discarded.
- **Chunk IDs never change.** Chunking is tier-independent: all tier models share the bert-base-uncased WordPiece vocab (verified), so one chunker tokenizer serves every tier. *This narrows the request that "chunk size scales with the tier": chunk size stays constant, and the TierProfile scales everything else.*
- **Sticky variant.** An index keeps the exact model file it was built with: the fp16 file if the build started on the GPU, the int8 file if on the CPU.
  - **fp16/fp32 files are EP-portable.** CPU vs DirectML cosine was ≥ 0.99998 on four models. An fp16 index keeps working on the CPU if the GPU disappears.
  - **int8 files are CPU-pinned.** DirectML int8 vs CPU int8 measured cosine 0.991–0.997, below the 0.999 bar, so an int8 index is always embedded and queried on the CPU.
  - Either way, an EP change never mixes embedding spaces and never forces a re-embed.
  - *Bundling the fp16 Minimum file (+64 MB) is what lets GPU machines on Minimum use the GPU with no download. The alternative is a CPU-only Minimum; that's the user's call if installer size matters more.*
- An iGPU never raises the tier. A CPU without AVX2 means Minimum regardless of the GPU, though the GPU is still used for Minimum.
- multilingual-e5-small stays a separate one-click profile, orthogonal to tiers.
- **Open gate (M1):** the bundled int8 variant (used on CPU-only machines) drifts from fp32 (top-10 neighbour overlap 0.855). If the eval harness shows a loss of more than 1.0 nDCG@10 versus fp32 on the golden set, ship fp32 instead (127 MB, about 25 % slower on CPU).
- **Lands in:** tier detection, TierProfile and the per-index model/dims schema in **M1**. Download, re-embed with atomic swap and the "Recommended" line in **M2**.

**P2 check.**
- [x] Can an average user do this without documentation? Yes: detection is silent, there is one "Recommended" line with [Use it], and no tier jargon (Basic / Better / Best).
- [x] Zero configuration on a fresh machine? Yes: the bundled Minimum model works offline, and nothing downloads without a click.
- [x] New options defaulted and out of the basic flow? Yes: the override is in Advanced.
- [x] Plain errors with one action? Yes: `ModelDownloadFailed`, `ModelIntegrity`, `DiskFull` and the new `ModelIncompatible` (#37) all keep the current search.
- [x] Security unchanged? Yes: models are sha256-pinned, and there is no new egress (downloads need consent as before).
- Risks logged: R21, R22, R23, R-UX14 (worse on CPU: see the measured speeds), R-UX16, R-UX17.

### Amendment 2: GPU on by default (partially reverses decision 5)

**Decision.** When a usable GPU exists, embedding runs on it **by default**, with no setting and no extra install. The execution provider is **ONNX Runtime's DirectML EP**.
- What ships app-local: `DirectML.dll` (Microsoft redistributable, 1.15.x) and, unless static ORT linking succeeds, `onnxruntime.dll` (Microsoft build 1.24.x).
- Windows ML is the post-v1 path. It uses the same ORT API, runs GPUs through DirectML on every supported Windows, and adds vendor EPs on 24H2+.

How the provider is chosen:
- **Dedicated GPU** (non-UMA, 4 GiB or more): DirectML.
- **Integrated GPU, or a dedicated GPU under 4 GiB:** a one-off timed probe decides. DirectML is used only if it runs at least 1.2× the CPU speed and its canary cosine against the CPU is at least 0.999. The result is cached per driver version.
- **WARP, or no D3D12 FL 11.0:** CPU.

**Silent CPU fallback ladder.** Failures never reach the user as errors. *Never rely on ORT's implicit fallback*: measured with an fp16 model, a DirectML registration failure without `error_on_failure` left a CPU session that failed to initialise (graph-optimizer cast error). Always register with `error_on_failure`, then build a fresh CPU session:
1. Register the EP with `error_on_failure`.
2. Commit the session.
3. Run a 2-chunk canary and check that the outputs are finite.
4. On any failure in steps 1–3, use a CPU session.

During a run, `DXGI_ERROR_DEVICE_REMOVED` rebuilds the CPU session and re-runs the batch. Three faults in 24 h keep the CPU until the driver version changes. Diagnostics get an `ep.changed` event.

**Good citizenship.** GPU batches are sized so each takes at most about 200 ms, and a batch over 500 ms halves the batch size. GPU indexing pauses while a fullscreen D3D app is in the foreground.

**Security (M8 kept).**
- DLLs load by absolute path only. `SetDefaultDllDirectories(SYSTEM32 | APPLICATION_DIR)` is called before ORT loads.
- `WinVerifyTrust` must confirm a Microsoft signature before the first load. Nothing is loaded by name from PATH or the current directory.
- Both DLLs measured Authenticode-valid. The decoy-DLL test (M0 test 4) is extended to `DirectML.dll`.

**Measured** on the dev box (RTX 5090, 512-token chunks):
- DirectML runs 43–57× faster than the CPU for the same model and precision, and 47× (Minimum), 59× (Standard) and 68× (High) faster comparing each tier's GPU file against its CPU file.
- fp32 and fp16 on DirectML match fp32 on the CPU: bge-small mean cosine 1.00000, min 1.00000 (requirement: ≥ 0.999).

**What still stands from decision 5:** figment stays dropped. "No DLLs" becomes "no DLLs loaded by name, and none the user installs".

**Lands in:** **M2**: the GPU path, the iGPU probe, the fallback ladder and the Advanced "Use graphics card: Auto / Off" switch. M1 runs on the CPU EP through the same `EpPlan` interface.

**P2 check.**
- [x] Can an average user do this without documentation? Yes: there is nothing to do, because the GPU is used automatically.
- [x] Zero configuration on a fresh Windows 10/11 machine? Yes: DirectML needs only a DX12 driver, which Windows Update installs. Without a GPU, the CPU is used silently.
- [x] Defaults sensible? Yes: the default is Auto, and the only option is an off switch in Advanced.
- [x] Any user-visible GPU error? No: the fallback is silent and shown only in diagnostics.
- [x] Security defaults unchanged and invisible? Yes: loads are absolute-path and signature-checked. The new DLLs are a new attack surface, logged as R17, and M0 test 4 is extended to cover them.
- Risks logged: R17, R18, R19, R20, R21. R-UX7 is closed differently.

### Amendment 3: M0 spike outcomes (orchestrator decisions, 2026-10-17)

Evidence: `research/m0-a-workspace.md`, `m0-b-native-stack.md`, `m0-c-extract-sandbox.md`, `m0-d-ipc-bridge.md`, `m0-e-gpu-tier.md`; independent verification in `m0-test-report.md`. Each item states what changed and why.

| # | Area | Plan said | Decision now | Evidence |
|---|---|---|---|---|
| 3.1 | C++ runtime | `+crt-static` everywhere, no VC++ runtime | **Engine (`sr-daemon`) links the dynamic CRT** and ships the four VC++ runtime DLLs (0.78 MB) app-local. `sr-bridge`, `sr-launch`, `sr-extract` and `sr-app` stay `+crt-static`. Still no runtime installer for the user. | B: prebuilt ORT is `/MD`-only; app-local CRT loads ahead of System32 and gives identical embeddings |
| 3.2 | Toolchain | MSVC ≥ 14.42 | **MSVC 14.44+ required** on dev and CI (ORT prebuilt uses symbols missing in 14.41 and older). | B, C |
| 3.3 | ORT loading | static, or load-dynamic fallback | Primary: ORT linked into the exe (no `onnxruntime.dll` lookup, so System32's Windows ML 1.17 copy is never touched). GPU DLLs (`DirectML.dll`) are **delay-loaded** by absolute path after `WinVerifyTrust`. Fallback if static+DirectML fails (test 14): `load-dynamic` of Microsoft's signed `onnxruntime.dll` 1.24.x by absolute path. | B: a planted `DirectML.dll` was loaded at startup until delay-load; E: load-dynamic path works |
| 3.4 | Bundled model | int8 (CPU) + fp16 (GPU), 96 MB | **bge-small-en-v1.5 fp32 only, on both CPU and GPU. File: Qdrant `model_optimized.onnx` (66 MB, fastembed's built-in), not `model.onnx` (127 MB)** — revised after the M0 tester (O3): both are fp32 and pass parity (min cosine 0.999998 vs 1.000000), so the smaller one halves the model's share of the installer. DirectML parity of this exact file is re-checked in M1. No CPU-pinned variant. int8 fails the 0.999 parity bar (B: 0.938; E: top-10 overlap 0.855). int8 returns for any tier **only** if the M1 eval harness shows ≤ 1.0 nDCG@10 loss. CPU-only Standard/High use fp32 until then. | B, E (resolves their disagreement on Amendment 1) |
| 3.5 | Chunking vs tier | chunk size scales with tier | **Fixed 400 tokens / 48 overlap for every tier**, so chunk IDs never change on a tier switch (shared WordPiece vocab verified). | E |
| 3.6 | High tier | dGPU ≥ 4 GiB | **Standard** = AVX2, 8 GiB RAM, 10 GiB disk, dGPU ≥ 4 GiB or ≥ 6 cores (arctic-embed-m-v1.5, 768). **High** = AVX2, 16 GiB RAM, 25 GiB disk, **dGPU ≥ 6 GiB** (arctic-embed-l, 1024). GPU batch capped by VRAM. | E (mxbai-large replaced: 54.4 vs 56.0 retrieval score) |
| 3.7 | ORT fallback | rely on ORT | Register DirectML with `error_on_failure`, then build a fresh CPU session on failure (ORT's implicit fallback breaks fp16 graphs). | E |
| 3.8 | ANN | IVF-PQ at > 50k rows | **IVF_HNSW_SQ by default** (recall@10 0.98 vs flat); IVF-PQ only as a memory-saving option (0.82 recall). | B |
| 3.9 | Daemon launch | bridge spawns daemon with breakaway, task fallback | **Bridge → short-lived `sr-launch` → daemon**, then the scheduled task as fallback. **Verified:** survives VS Code's `taskkill /T /F`. **Not yet verified (M0 tester O1):** nested Jobs that forbid breakaway. The spike launcher never checks whether the daemon is still inside a Job, so that case currently fails silently. M1 adds the check (`IsProcessInJob` on the daemon; if still jobbed, use the scheduled task) and a nested-Job test. | D (11/11 races one daemon; launcher 3/3, survives tree kill); tester 9/9 races |
| 3.10 | Pipe security | user-SID DACL | DACL **plus a mandatory integrity label** (no low-IL read access), `PIPE_REJECT_REMOTE_CLIENTS`, name derived from user SID + data-dir hash. | D |
| 3.11 | Extractor sandbox | low-integrity token | **AppContainer with zero capabilities** (no file reads, writes or network) as primary; low-integrity as fallback. Worker created with no console (`CREATE_NO_WINDOW` was the crash cause). Supervisor timer is the real timeout; Job CPU limit is a backstop; 1 GiB memory cap; recycle after high-memory jobs. | C (43 hostile scenarios × 5 configs, no escapes, no leaked workers) |
| 3.12 | Data directory | `%LOCALAPPDATA%\SR` | Must be a **short, plain local path** (no UNC, no `\?\`, < 260 chars); the installer controls it and the daemon refuses others with a plain error. All LanceDB calls are wrapped in `catch_unwind`. | B (LanceDB fails or panics on long/UNC/verbatim paths) |
| 3.13 | TLS | rustls | Install **one rustls crypto provider** at process start (two backends get pulled in; otherwise a runtime panic). Accept duplicate reqwest/tower-http majors from LanceDB for now (size only, nothing listens); track as R24. | B |
| 3.14 | Extract hardening (M1) | — | M1 adds: HTML nesting-depth cap (2 MiB nested HTML took > 10 s), better binary sniffing incl. HTML, never trust pdfium page count, extracted text sent as a raw frame instead of JSON. | C |
| 3.15 | Installer size | not estimated | ≈ 106–114 MB download, ≈ 285 MiB installed (LanceDB ≈ 159 MiB of it). Accepted; size-optimised profile tracked as R25. This figure assumes the 66 MB model file from 3.4. | B, tester |
| 3.16 | `+crt-static` placement | global `.cargo/config.toml` | Set **per package**, not globally (cargo merges config by directory, so a global flag would also hit `sr-daemon`). Until set, every workspace exe imports `VCRUNTIME140.dll` and would fail on a clean machine. Tester proved `sr-extract` built `+crt-static` imports only system DLLs and passes the hostile corpus. Fixed at the start of M1. | tester O2 |
| 3.17 | Uninstall | — | Uninstall must also delete the extractor's AppContainer profile and its ACL entries. Added to M2's uninstall-cleanup exit. | tester |

**P2 check.** None of these adds a user-visible step: the CRT DLLs, launcher, sandbox and data-dir rule are all installer- or daemon-internal; the bundled model still works offline with no download; GPU use and its fallback stay silent. The only user-visible effect is a larger installer (3.4, 3.15).

### Amendment 4: M1 wave-1 outcomes (orchestrator decisions, 2026-10-18)

Evidence: `research/m1-a-catalog.md`, `m1-b-extract.md`, `m1-c-client-bridge.md`, `m1-d-providers.md`, `m1-e-core.md`, `m1-f-eval.md`; follow-ups in `m1-wave2-integration-notes.md`.

| # | Area | Plan / Amendment 3 said | Decision now | Evidence |
|---|---|---|---|---|
| 4.1 | CRT for static programs | literal `+crt-static` (3.16) | **Hybrid CRT**: VC runtime linked statically, UCRT from Windows (inbox since Win10). `sr-bridge`, `sr-launch`, `sr-extract`, `sr-app` import system DLLs only. No user-installed runtime. CI `check-crt` enforces it. | A |
| 4.2 | Startup replay | replay outbox | Required order: catalog open → `start_epoch` → per-store replay from watermark → **`finalize_applied`** → pipe opens last (open pipe = ready). Closes a "pending forever" crash window. | A, C, E |
| 4.3 | Lance watermark | table metadata in the same commit | lancedb 0.40 can't attach metadata to `merge_insert`; watermark is a **sentinel row** written in the same commit. Same atomicity. | E |
| 4.4 | Crate deps | sr-core uses catalog/providers | sr-core defines narrow traits (`OutboxSource`, `ChunkSource`, `Embedder`, `Reranker`); sr-engine adapts. Supervisor is its own crate `sr-extract-client`. | B, E |
| 4.5 | Extract scope | PDF/Office in worker | **All formats** (text, Markdown, code too) go through the sandboxed worker (~0.3 ms/file). html2text replaced by a single-pass linear tokenizer. Install-dir ACL (inheritable ALL APPLICATION PACKAGES RX) checked in every sandbox mode; never per-package ACEs. | B |
| 4.6 | Scheduled task name | `SR\daemon` | **`\SR\daemon-<sid8>`** per user (no cross-user collision). Bridge drains in-flight replies up to 2 s before exit. Nested-Job check implemented (closes 3.9 gap). | C |
| 4.7 | Partial-scope ANN | filter in ANN | Whole-collection scopes skip the filter; partial scopes **widen the pre-filtered search** in proportion to scope size; ≤ ~1–5 % scopes use exact scan. ANN defaults nprobes 16, ef 100, refine 5. | E (recall 0.58 → ≥ 0.997) |
| 4.8 | DLL hardening | delay-load in build | Delay-load must be in **`sr-daemon`'s own `build.rs`**, and `harden_dll_search()` is the first statement of `main()`. Loader refuses unsigned/renamed DirectML/d3d12. | D |
| 4.9 | Eval gate | nDCG ≥ Selene V2 | Gate = Selene V2 as shipped (bge-large, file-level nDCG@10 0.741 selene-src, 0.275 CQADupStack-webmasters). Standard BGE/arctic usage (CLS pooling, query-only prefix). RRF weights tuned against the harness, not assumed. | F |
| 4.10 | Tier settings home | unspecified | User choices in `settings.toml`; detected facts in catalog `meta`. One home per setting. | orchestrator |
| 4.11 | Startup order (amends 4.2) | replay → `finalize_applied` | Required order is now: catalog open → `start_epoch` → per-store replay → **copy each store's own watermark into the catalog cache** → `finalize_applied` → sync start → pipe opens last. Without the copy step, a crash after both stores commit but before catalog ack left files "pending" forever. | W2-E |
| 4.12 | MCP service crate | rmcp service in `sr-engine` (D5) | `PipeService` + rmcp `ServerHandler` live in **`sr-mcp-server`** (library + `sr-daemon` bin); `sr-engine` stays rmcp-free to avoid a dependency cycle. | W2-E |
| 4.13 | GPU detection timing | tier detection includes GPU | M1 detects RAM, cores, AVX2, free disk and plans for CPU; **DXGI GPU enumeration lands in M2** with the GPU-default path (consistent with Amendment 2). | W2-E |
| 4.14 | Data-dir ACL | protected ACL | Set with System32 `icacls` by absolute path; daemon **refuses to start** if it fails (plain message + "Repair" action in M2 UI). | W2-E |
| 4.15 | Folder status | cached derivation | Computed live from catalog on read (a cached value lagged one refresh). | W2-E |
| 4.16 | Supply chain | cargo-deny clean | Two `unmaintained` (not vulnerable) exceptions, `paste` (build-time only) and `unsafe-libyaml` (LanceDB's own config), both transitive via pinned lancedb 0.40; review date 2027-01-18. | W2-H |
| 4.17 | Rename → search path | rename keeps file_id/chunk IDs (m1-e O3 left lexical path stale) | A pure rename enqueues `index_file {"path_refresh":true}` (scan **and** watcher): the engine re-applies the file at its current version, so BM25 finds the new name and not the old, with **no re-embedding**. Racy renames are hashed against the renamed row and stay path refreshes when unchanged. Coalescing now **merges payloads** (`jobs::merge_payload`: `force` sticky; path-only only if both sides are; otherwise a full job flagged `renamed`). The old `coalesce(new, old)` would let a path refresh swallow a content edit. | CO-1 |
| 4.18 | Tier settings home (completes 4.10) | `meta.tier_override` and ad-hoc `settings.toml` keys | `[search] tier_override` (`auto`/`basic`/`better`/`best`) and `use_graphics_card` (default on) are **typed** in `sr_common::config::Search` (Advanced, restart). Bad values fall back to defaults with a notice. Catalog `meta` tier-override helpers are **removed**. ANN defaults have one source, `sr_common::tier::ANN_DEFAULT`, from which sr-core `AnnConfig` derives. rustls is pinned once in the workspace, ring only, and deny bans aws-lc. | CO-1 |
| 4.19 | Job ids | `jobs.id INTEGER PRIMARY KEY` | Catalog schema **V2**: `jobs.id AUTOINCREMENT`, sequence seeded past `job_history`. V1 reused ids once the queue drained, so `job_history` recorded new jobs under old rows (wrong kind and payload). | CO-1 |
| 4.20 | Scan vs running job | scan coalesces into any live job | A `pending_index` file with no hash and nothing changed only **ensures** a job exists (`jobs::ensure`). It no longer flags a running job for a redundant second index. Changed or renamed files still coalesce with `rerun`. | CO-1 |
| 4.21 | grep scope | `ignore` walker defaults | `grep` honours ignore files **inside** the folder only; `.gitignore` files above the root are ignored (Selene's root `.gitignore` hid indexed `docs/`). | M1 tester |
| 4.22 | Folder status push | status refresh on scan | The status publisher uses the shared catalog `changed` flag after **every** writer commit and records `last_success_at` (the final "synced" was never pushed; `sync_folder` hung). | M1 tester |
| 4.23 | Idle exit | 10 min after last client + empty queue | Also requires an **empty writer backlog** (outbox), so a CPU-only first index finishes with no client attached. | M1 tester |
| 4.24 | Readiness vs replay (amends 4.2/4.11), tester item **B** | pipe opens after replay | **Pipe opens before replay.** Ready = catalog migrated → `start_epoch` → stores opened → store watermarks copied → `finalize_applied` (all cheap) → pipe opens. Pending outbox work is then replayed as the writer's first background batches. Search serves the last committed state (query-time version validation already hides divergence); status shows "Catching up after restart" with progress. Removes a measured 20.8 min "Background service didn't start" window. | M1 tester; orchestrator decision |
| 4.25 | Group-commit bound (amends §2.3/§2.7), tester item **C** | 2 s or 1k docs | A commit closes at whichever comes first: **2 s wall time, 512 embedded chunks, or 1 000 outbox ops**. Embedding streams through `EmbedBatcher`; each closed batch commits (Lance → tantivy → T2). Shutdown drain stops at the next batch boundary (target ≤ 10 s) and leaves the rest in the outbox. Removes ≈ 20 min CPU commits, frozen progress and the failed 180 s drain. | M1 tester; orchestrator decision |
| 4.26 | Bundled model variant (3.4 follow-up) | int8 allowed if ≤ 1 nDCG point lost | int8 passes on both corpora (+0.24 / +0.08), but **fp32 stays the single bundled file**: one embedding space for CPU and GPU, CIs are ±1 point, recall@50 −1.9 on CQADupStack. int8 may return later as an opt-in CPU-only speed variant. | M1 tester; orchestrator decision |
| 4.27 | Query embedding device | GPU when present | Queries always embed on the **CPU** (p50 11.1 ms vs DirectML 26.7 ms for one short query); the GPU is used for indexing batches. | M1 tester |

**P2 check.** No user-visible step added. The only user-facing text introduced is the version-skew message "Restart Claude to finish." (4.6), which is plain-language with one action.

### Amendment 5: Owner decisions on M2 delivery (user, 2026-10-18)

- **5.1 CI is active.** The workflow is copied to `<repo root>/.github/workflows/selene-retrieval-ci.yml` (the only file outside `projects/selene-retrieval/`, approved by the owner). `projects/selene-retrieval/.github/workflows/ci.yml` stays the source of truth; the two must stay identical below the header. It runs once committed and pushed to `main` or in a PR; agents don't commit.
- **5.2 Code signing is deferred.** The owner has no signing account yet. M2 ships an **unsigned** installer; the signing hooks stay in place and sign automatically once credentials exist (they never fake a signature). Consequences: SmartScreen shows "Windows protected your PC" on first run, and "Microsoft-signed only" policies on managed PCs will block it. Mitigation for the beta: the download page and the M2 user test brief explain "More info → Run anyway" with a screenshot (R-UX6). Signing returns as a gate before general availability (M5), not the beta.
- **5.3 Clean-VM checks: the orchestrator chooses the method.** Windows Sandbox isn't installed on the dev machine, and enabling it or Hyper-V needs admin. The checks therefore run as:
  1. An **`installer-smoke` CI job** on a GitHub-hosted Windows runner (a fresh VM per run, different user, no prior app state). It installs the unsigned setup silently and runs `installer/verify` (install, MCP round trip, uninstall with and without the index, nothing left behind), plus the zero-config first run with a network-endpoint watch. Limitation: hosted runners already have the VC++ runtime, so the "no runtime needed" proof stays the `check-crt` and app-local DLL import checks (Amendments 3.1 and 4.1).
  2. The `installer/verify/clean-vm.wsb` kit stays ready for a pristine Win10/Win11 run, MSIX Claude Desktop, Cursor and a second user, whenever an admin-enabled Sandbox or VM is available. Until then these remain pending in `04-roadmap.md`.
- **5.4 The M2 user test is run by the owner:** three non-technical users, unmoderated, per the M2 first-run column. The agents prepare the test kit: a one-page task card, the success criteria (download → search from Claude in under 5 minutes, no docs), a short observation form, and the SmartScreen note from 5.2. M2 is "done (pending user test)" until the owner reports results; every failure becomes an R-UX entry.

### Amendment 6: M2 close-out (orchestrator decisions, 2026-10-18)

Evidence: `research/m2-test-report.md`, `research/m2-test-progress.md`, `research/m2-build-speed.md`.

| # | Area | Before | Decision now | Evidence |
|---|---|---|---|---|
| 6.1 | Test switch `SR_NO_GPU_PAUSE` | R19's full-screen back-off (`SHQueryUserNotificationState` busy / D3D full-screen / presentation → GPU indexing paused) applied everywhere, including tests that run the real daemon on a shared desktop | New **dev hook** `SR_NO_GPU_PAUSE` read through `sr_client::naming::dev_env` in `sr_engine::gpu` `apply()`: when set, the GPU pause callback isn't installed. Like every dev hook it is **ignored in release builds** (`cfg!(debug_assertions)`), so the shipped behaviour is unchanged. The `sr-mcp-server` daemon tests set it and now assert wait-idle `"idle": true` instead of ignoring `idle:false`. No user-visible effect. | g4 test2 failure (1-file index waited > 300 s behind a full-screen window); g5 ×2 and g6 pass with a full-screen app in front |
| 6.2 | Updater `zip` feature | `tauri-plugin-updater` 2.14.0 with default features | `default-features = false, features = ["rustls-tls", "system-proxy"]` (workspace `Cargo.toml` dependency line; no profile change). The default `zip` feature pulled zip 4.6.1 with RUSTSEC-2026-0367/0368/0369 and failed `cargo deny`. Not needed: the Windows v2 updater downloads and runs the NSIS installer; zip is only for `.zip`/`.tar.gz` bundles, which we don't ship. zip is gone from `Cargo.lock`. Re-check if a non-NSIS update format is ever added. | g2 deny failure → g3 deny OK |
| 6.3 | Installer build profile (R25) | `dist` (opt-level 3, cgu1): 121.5 MB installer / 334.1 MiB installed | **Keep `dist` as the installer profile.** Rule (orchestrator): identical nDCG and every speed metric within 5 % → `dist-size`, else keep `dist`. `dist-size` (opt-level "s") gives **107.0 MB / 247.4 MiB** (−14.5 MB / −86.7 MiB) with identical nDCG@10 on selene-src (0.897), and CQADupStack (0.363), equal hybrid search (1M p50 21.0 vs 20.8 ms) and query latency (+3 %), but full-engine **GPU indexing is 10.5 % slower** (357 vs 398 chunks/s, two clean runs each) and Lance `merge_insert` 9–61 % slower. R25 is set to **~121.5 MB / 334 MiB**. `xtask package --opt-size` stays available as an experiment; no `Cargo.toml` profile change. | `m2-test-report.md` §3.1 (idle-gated, paired o3/os runs) |
| 6.4 | GPU pause is silent (R-UX32) | R19 pauses GPU indexing with no status text and no end while any full-screen window is open | **Logged as risk R-UX32, fix scheduled for M3, not implemented in M2:** a plain status line "Waiting while a full-screen app is open", and after 5 minutes of pause continue on the CPU (existing fallback path, same index and embedding space, no setting touched). M2 ships the R19 behaviour unchanged. | g4; this session's measurements had to skip DirectML index runs while a full-screen video was open (§3) |

**P2 check.** 6.1–6.3 add no user-visible step. 6.4 records a P2 failure (an unexplained wait) as R-UX32.

### Amendment 7: M3 close-out (orchestrator decisions, 2026-10-19)

Evidence: `research/m3-p-providers.md`, `m3-q-ranking-answer.md`, `m3-u-app.md`, the independent tester
`research/m3-test-report.md` (checkpoints `m3-test-progress.md`), and `m3-design-spec-intake.md`.

| # | Area | Before | Decision now | Evidence |
|---|---|---|---|---|
| 7.1 | genai features | workspace pin with default features | **`genai = { version = "=0.7.0-rc.8", default-features = false }`** (root `Cargo.toml` dependency line, no profile change). Its default `rustls-tls` enables `reqwest/rustls` = aws-lc, which is banned (3.13). genai gets our ring-only reqwest client. Also pinned: `ReasoningEffort::Budget(0)` on OpenAI-compatible calls, because genai otherwise truncates model ids ending `-max/-low/-high`. | M3-P D1/D8; gate `cargo deny` OK |
| 7.2 | Provider list | page 20 named OpenAI, Gemini, Ollama | **Selene's registry is the only source:** `cargo xtask sync-providers` writes `crates/providers/data/selene-providers.json`, and CI runs `sync-providers --check` (both workflow copies, which also trigger on Selene's `lib/ai/provider-types.ts` and `lib/ai/providers.ts`). A new Selene provider appears **disabled** (`not_supported_yet`) until a policy row (protocol, base URL, validation) is added. | intake; M3-P D4/D5; tester CI step |
| 7.3 | OAuth / subscription providers | C2 open | **Listed but disabled** ("not available yet", `oauth_pending_design`): Codex, Claude Code and Antigravity, until a design pass and a token store exist. API-key and local providers (Ollama, vLLM) work. Logged as P2 gap R-UX38. | M3-P, M3-U; intake C2 |
| 7.4 | WebView2 | M2 bundled the bootstrapper; M3-U switched the Tauri config to `offlineInstaller` | **Online bootstrapper by default** (1.8 MB, Microsoft-signed, run only when WebView2 is missing). The **offline variant** is a separate installer: `cargo xtask package --webview2-offline` → `SeleneRetrieval-<v>-x64-offline-setup.exe` with the standalone runtime (`SR_WEBVIEW2_OFFLINE`, ≈ +180 MB, same Microsoft-signature rule). `tauri.conf.json` is back to `embedBootstrapper`. Publishing the offline variant and testing it on LTSC stay in M5. | owner default; tester |
| 7.5 | System proxy | reqwest `system-proxy` (unverified) | **Own resolver:** override → the user's Windows settings incl. WPAD/PAC (`WinHttpGetProxyForUrl`, `winhttp.dll` resolved from System32) → machine WinHTTP → direct. Loopback is always direct. Proxy credentials (Basic) live in Credential Manager. **NTLM/Kerberos proxy sign-in is not supported** (R-UX34), so the M3 exit "authenticated corporate proxy" holds for Basic and PAC/WPAD only. WPAD before Ready is risk R32. | M3-P D3; contract tests; tester review of `proxy::win` |
| 7.6 | Consent | per purpose | Per (provider, purpose), **also for local servers** (D7). The key-check click is the consent for `validate_key` + `list_models` (D-35). `answer` covers `iterate`. "Don't allow" is now recorded (`consent.set allowed:false`) and clears the pending request, so a reconnect doesn't ask again and the next blocked call asks afresh (tester fix). Provider errors carry the shared `ErrCtx` (provider, host, `secs` from Retry-After). | M3-P, M3-Q, M3-U; tester wire audit |
| 7.7 | Answers and enhance | — | Answer loop = **JSON protocol over plain chat**: up to 6 reads, citations validated against the shown sources, untrusted text fenced, overall timeout 150 s. **No provider call when nothing matches.** Deeper answers are off by default (1–5 rounds). The enhancer is a byte-for-byte port of Selene's V2 request; it falls back to the heuristic prompt also when the provider didn't respond, and uses our no-good-match cut-off for its context. | M3-Q §1.3, deviations 1–3; parity test; live checks |
| 7.8 | Reranker | Advanced download, consent | Engine-side local cross-encoder (`ms-marco-MiniLM-L-6-v2`, pinned, 91 MB), **off by default and not surfaced in the app for M3**: measured −4.8 nDCG on selene-src (path queries −30) and ≈ 2.5 s per search on the CPU. The High tier's `rerank` flag does not turn it on (that would download without a click). Tuning (top-N, skip path/identifier queries, GPU) and the Advanced switch come in M4 (R-UX36). | tester evals |
| 7.9 | Exact identifiers and numbers (R-UX33) | M3-Q exact lane | **Exact lane on by default.** Rules: a whole exact term vouches for a hit on its own. A digits-only query, or a query that is exactly one identifier / compound, with no whole-term hit is "nothing matched" (tester). Whole-term hits rank first for single-identifier / digits-only queries (tester). Result: selene-src 0.918 (≥ 0.897), 23/23 exact positives at rank 1, 16/16 exact negatives. Cost: −1.6 to −2.7 nDCG on natural-language queries (R-UX37, tune lane weight in M4). | tester evals and the 76666 live repro |
| 7.10 | Multilingual model | profile, M2 path | Engine-side static profile (`tier: Minimum` resource class, catalog `custom`). **Tokenizer pins are looked up by model id** (`sr_providers::models::tokenizer_pins_for`; the e5 pins live in sr-providers and the engine asserts they are equal; P-2, tester fix). The page 12 offer UI is M4 (R-UX39). | tester live switch (40/40 searches during it) |
| 7.11 | Approval launch (R-UX30) | not implemented | The daemon starts `<install dir>\sr-app.exe --approve <id>` by absolute path when no app session is attached (0.5 s grace, once per request). No `--data-dir` is passed: the installed layout uses the default data dir. Focus goes to the card heading, never a button. | M3-Q, M3-U; tester on the installed layout |
| 7.12 | Lint policy | `-D warnings` everywhere | Unchanged, with two scoped exceptions: `#![allow(clippy::result_large_err)]` in sr-app `commands.rs` / `updater.rs` (`UiError` is the Tauri IPC error, built once per user action), and MCP `eng()` returns `Box<CallToolResult>`. | gate g1/g2 |
| 7.13 | Build cadence (M3 record) | one gate, one fix batch, one rerun | Followed. g2 still needed 3 clippy fixes in crates g1 could not reach (their dependency failed to build); they were confirmed by workspace clippy plus targeted tests, without a third full gate. **Three ranking / text fixes found by evals and live checks after the single dist build are not in the M3 installer**; they were verified with a release bench and daemon plus targeted tests (130/0), and the next dist build picks them up. | `m3-test-progress.md` |

**P2 check.**
- [x] Average user without documentation: yes for the basic flow. Answers are optional, the key is pasted once and validated with plain errors, approvals open the app, and the full-screen wait explains itself. **Failures logged:** R-UX38 (where to get a key; subscription-only users) and R-UX39 (features with no UI yet).
- [x] Zero configuration: the first run makes 0 network endpoints, writes default settings and lists no provider tools.
- [x] Defaults: answers on only after a provider is added; deeper answers, reranker and MMR off.
- [x] Plain errors with one action: unchanged codes, now with the provider's real wait time.
- [x] Security unchanged and invisible: Credential Manager keys, consent before every request, egress audit, ring-only TLS, scoped answer reads. **New risks:** R31 (same-user app pipe), R32 (WPAD), R-UX34 (NTLM).

### Amendment 8: M3 open questions decided, M4 start (orchestrator decisions, 2026-10-19)

The owner directive of 2026-10-18 lets the orchestrator decide anything that does not need a new cargo-deny exception or a change outside this folder. These are the tester's M3 questions.

| # | Question | Decision | Why |
|---|---|---|---|
| 8.1 | Reranker | **Stays off and hidden through M4.** Tuning moves to M5 "performance tuning". It is surfaced (Advanced only) only if it beats rerank-off on selene-src and CQADupStack and adds ≤ 300 ms p95. | M3 measured −4.8 nDCG (path queries −30) and +2.5 s per search. |
| 8.2 | NTLM / Kerberos proxies | **Not in v1.** Supported: no proxy, Basic, PAC, WPAD. The proxy error copy says plainly that sign-in proxies are not supported yet. Re-assess in M5 with the proxy VM and the external security review. | A Negotiate transport is a large, security-sensitive change; it doesn't belong in the feature milestone. R-UX34 stays open. |
| 8.3 | Subscription sign-in (Codex, Claude Code, Antigravity) | **Stays disabled in M4.** Before any design work, M5 checks each provider's terms on third-party use of subscription tokens. Until then, subscription-only users are pointed to local Ollama / vLLM or an API key; page 21 gets a "Where do I get a key?" link per provider (closes R-UX38's first half). | Using consumer subscription tokens from a third-party app may break provider terms; that can't be undone after release. |
| 8.4 | Post-dist M3 fixes | Not in the M3 installer. They ship in the M4 `dist` build, and the M4 tester re-runs the install round trip and the `76666` repro on it. | Build cadence: `dist` once per milestone. |
| 8.5 | M4 split | Two build agents with disjoint folders: **M4-E** (engine, MCP server, client, catalog, sync, common) and **M4-A** (`crates/app`, `ui/`, `installer/sr-setup`). M4-E publishes the RPC/event contract first in `research/m4-rpc-contract.md`; M4-A codes against it. One M4 tester runs the gate. | Two folders don't overlap; more agents would fight over the router, bindings and `commands.rs`. |
| 8.6 | Disk | The M4 tester may delete regenerable `target/m2-*` and `target/m3-test/{debug,release}` build output before the gate. Logs, reports and `package/` stay. | C: is 97 % full (32 GB free); one gate needs about 40 GB. |

### Amendment 9: M4 close-out (orchestrator decisions via the M4 tester, 2026-10-19)

Evidence: `research/m4-test-report.md` (checkpoints `m4-test-progress.md`), `research/m4-e-engine.md`,
`research/m4-a-app.md`, `research/m4-rpc-contract.md`. None of these needs a new cargo-deny exception or a change
outside this folder.

| # | Area | Before | Decision now | Evidence |
|---|---|---|---|---|
| 9.1 | App time limit for Ask / Prompt Enhancer | Every app-RPC call had a 30 s limit; on timeout the app dropped the call, but the service kept calling (and billing) the provider | `answer` / `enhance_prompt` wait **180 s** (above the engine's own 150 s cap, 7.7); if that still runs out the app sends `requests.cancel {request_id}`. Every other call keeps 30 s. | P2 review; `crates/app/src/daemon.rs` `timeout_for` + unit test |
| 9.2 | Engine features without UI (R-UX39) | multilingual model, deeper-answer rounds reachable over RPC only | **Built:** Advanced → search model "Multilingual search" (236 MB GPU / 471 MB CPU download); the page 12 offer line only when the Windows display language isn't English (`navigator.language`); Advanced "Extra search rounds for deeper answers" 1–5. The reranker stays hidden (8.1). The snapshot's `tier.multilingual` is now used (`TierView.multilingual`), so a multilingual index never shows as "Basic". | P2 review; bindings regenerated |
| 9.3 | Per-app scope picker | choosing "Only some folder groups…" saved an empty list at once (app cut off by a misclick) | Nothing is saved until the first group is ticked; the app keeps "All my folders" meanwhile, and the line says so. Deleted / unknown groups still fail closed (engine). | P2 review; live scope checks |
| 9.4 | "Where do I get a key?" (R-UX38, 8.3) | `key_url` missing from the manifest | `key_url` per provider in the `xtask sync-providers` policy table (vendor key pages; Ollama / vLLM: download / install page; OAuth rows none). The app opens only exact `https://` URLs from the list. Re-check the URLs when Selene's registry changes. | xtask; manifest regenerated; `sync-providers --check` OK |
| 9.5 | Page 31 [Change search model] | opened Settings (top) | Opens Settings with `section=search`: focus on "Search quality", Advanced (where the model choice lives) opened. | step 1c |
| 9.6 | Uninstall test with client configs | `roundtrip.ps1` uses the real profile (metadata-only check of Claude / Cursor / VS Code files) | Kept for the installer round trip (the uninstaller only writes when our entry exists; size + mtime verified unchanged; the tester never reads the files). The **entry-removal behaviour** is tested live against **fake roots** with a new debug-only hook `SR_CLAUDE_ROOTS_DIR` in `sr-setup` (ignored in release, like every `dev_env`). | round trip 49/49; fake-roots 7/7 |
| 9.7 | Moved index missing at start (R34) | — | Accepted as designed: the service refuses to start rather than fork an empty index. The wrong next action in its text (added-folder sentence) is a P2 gap for M5. | live check, exit 3 |
| 9.8 | Risk ids | M4-E and M4-A both proposed "R-UX40" | M4-E's (search paused during an index move) is **R-UX40**; M4-A's ("Open" not offered) is **R-UX41**. M4-E's R37 (clippy not run) is closed by the gate. | 04 risks |
| 9.9 | M4-E new dependencies (D7) | declared in crate manifests, not the root `[workspace.dependencies]` | Kept as is for M4 (no root manifest change during the gate; `cargo deny` accepts `httpdate`). Move them to the workspace table in M5 hygiene. | gate deny OK |
| 9.10 | Activity retention boundary | test assumed `ts <= now − 7 d` is pruned | `ts < now − 7 d` is pruned (same boundary as the 7-day page filter); the test was wrong. | g1 test fix |
| 9.11 | R25 band in `xtask package` | M0 estimate 106–114 MB / 285 MiB | 121–124 MB / 339 MiB (measured M2/M3); M4 dist 123.7 MB / 344.2 MiB. | dist build |
| 9.13 | selene-src quality gate | M3 recorded 0.918 (exact lane on) | M4 measures **0.9173** vs M3's unrounded **0.9177** (−0.04 pts): 8 identifier / path queries moved, 4 up and 4 down, consistent with the post-M3-dist sr-core ordering fixes that M3's eval predates; lane-off is identical but for one path query (GPU-built index). Accepted (**PASS-with-decision**); the gate stays "no lower than the last milestone by more than run-to-run noise" and ≥ 0.897 (M1). M4 changed no ranking code. | `logs/e-src.*`, per-query diff |
| 9.12 | Build cadence (M4 record) | — | Followed: one gate (g1), one fix batch, one rerun (g2, all green), `release` once (gate) + the release eval tools, `dist` once. Two clippy-only passes between g1 and g2 reached crates that g1's clippy couldn't (a failing dependency stops `--keep-going` for its dependents); recommended for every gate. | `m4-test-progress.md` |

**P2 check.**
- [x] Average user without documentation: the five M4 user-test tasks are reachable without Advanced (P2 review). Fixed gaps: 9.1–9.3, 9.5. **Pending:** the 3-new-user test (owner).
- [x] Zero configuration: clean first run 0 network endpoints; HTTP off; scopes "All my folders".
- [x] Defaults: HTTP, folder groups, index move, rounds, multilingual all Advanced / opt-in.
- [ ] Plain errors with one action: **gap R34** (missing moved index uses the added-folder sentence); R-UX43 wording.
- [x] Security unchanged and invisible: HTTP loopback + per-tool token + Host/Origin guard; R31 peer check on release; scopes fail closed; protected ACL on a moved index. Residuals: R31 until Authenticode, R36.

### Flagged as unverified (resolved in M0 unless noted)
- Whether ORT prebuilt static libraries and pdfium link with `+crt-static`.
- Whether lancedb 0.40 can write metadata atomically with `merge_insert` (the design no longer depends on it).
- Whether `default-features=false` on lancedb drops reqwest 0.12 transitively. Whether axum's optional tower-http 0.6 stays out of the build.
- grep-regex, specta-typescript and tree-sitter grammar versions; whether ulid 3.0 has a serde feature.
- bge-small int8 size, and fastembed user-defined CLS pooling with the prefix.
- Whether reqwest reads the Windows system proxy natively (M3). **Resolved (Amendment 7.5):** it reads the registry only (no PAC/WPAD, no `netsh winhttp`), so sr-providers has its own WinHTTP resolver.
- How Claude Desktop MSIX treats Job Objects and child processes (M0 test 6). Config path virtualisation and config wipes are confirmed: claudeissues #26073, #25579, #34359; exchangepedia (2026-04).
- Whether the Tauri NSIS updater can replace a running `sr-bridge.exe` (M2).
- CPU embedding throughput (M1).
- Current stable Rust. 1.96.1 (June 2026) is confirmed; a later version is inferred from the six-week cadence.

### Selene files that matter for implementation
- `lib/db/sqlite-character-schema.ts`: parity source for folder settings and the M6 importer.
- `lib/ai/filesystem/path-utils.ts` and `read-utils.ts`: behaviour `read_file` must match.
- `tsconfig.json`, `vitest.config.ts`, `vitest.integration.config.ts`, `.fallowrc.json`, `fallow.jsonc`, `.gitignore`: where the `projects/**` excludes go.

### Amendment 10: Owner decision on Claude Code sign-in (user, 2026-10-19)

| # | Topic | Decision | Notes |
|---|---|---|---|
| 10.1 | Claude Code subscription sign-in (**revised 2026-10-19, second owner decision**) | **Owner overrides the M5-S recommendation.** Claude Code is *not* marked `not_allowed_by_vendor` and is *not* limited to API keys. Its provider entry is `enabled` with sign-in via Agent SDK OAuth. **Sign-in is on by default in every local and private build**; there is no default-off flag for these builds. The settings entry stays, labelled "Claude (sign in)". Antigravity stays API-key only, and Codex stays `pending_vendor_approval`. | The owner states Anthropic allowed this sign-in for the Selene app, that Selene Retrieval inherits it, and that the owner is the only user and will not ship it publicly. The Agent SDK overview (code.claude.com/docs/en/agent-sdk/overview, checked 2026-10-19) says third-party claude.ai login needs Anthropic's prior approval; the owner's statement is that approval. **Release gate (R41), narrowed:** applies only to a public or distributed build. Before any such build, recheck the feature against Anthropic's Agent SDK terms then in force and keep a record of approval with the release notes. Private builds are not blocked. **Implementation:** reuse Selene's existing Claude Code login flow, not a second design: the states and copy of `app/settings/settings-panel.tsx` (login, browser opened, paste mode with submit/cancel, auth success, logout, account email) and the token handling in `lib/auth/claudecode-auth.ts` plus `app/api/auth/claudecode/{authorize,exchange,refresh}`. The Tauri app can't call Selene's Next.js routes, so the Rust side mirrors that protocol 1:1 (read-only reference, Selene files untouched), with tokens in Credential Manager under `SeleneRetrieval/`. It appears in the wizard step "Get written answers (optional)" (page 20) and in Providers & Keys (page 21). Branding follows the SDK rules: "Claude (sign in)", no Claude Code look-alike visuals. |
| 10.2 | Claude (sign in) reuses the local Claude Code login (**owner decision 2026-10-19, M5-C; supersedes the implementation part of 10.1**) | "Claude (sign in)" no longer runs its own OAuth (no PKCE, no pasted code, no refresh, no tokens in Credential Manager). Like Selene's `sdk` backend (the Agent SDK runs the Claude Code CLI, which owns the credentials in `~/.claude`), Selene Retrieval runs the user's own `claude` CLI: status only from `claude auth status --json`; answers / enhance through `claude -p` with the prompt on stdin, `--output-format stream-json`, `--model`, `--tools ""`, `--strict-mcp-config` (no `--mcp-config`), `--setting-sources ""`, `--disable-slash-commands`, `--no-session-persistence`, our own `--system-prompt`, an empty temp working folder, no console window, a Job Object around the process tree (cancel / 180 s app limit kill it). The CLI is found by absolute path only (`%USERPROFILE%\.local\bin\claude.exe`, `%APPDATA%\npm\claude.cmd`, then absolute local PATH folders; never the current directory). Not installed → "Claude Code isn't installed on this computer" + link https://claude.com/claude-code; not signed in → "Claude Code isn't signed in" + [Sign in], which opens the CLI's own `claude auth login` in its own window (it opens the browser itself); the card re-checks when the app window regains focus, on [Check again] and when that window closes (pushed). **On by default** in every local and private build; label stays "Claude (sign in)"; consent (page 22) still applies before the first send. Selene Retrieval never reads `~/.claude/.credentials.json` or any token, and never signs the user out of Claude Code (removing the provider only disconnects it). | RPCs: `cli_signin.{status,start} {provider}` and `claude_signin.{status,start}` (= `claudecode`), privileged (R31); `submit_code` / `logout` removed; event `cli_signin.changed`. Old `SeleneRetrieval/claude-signin` entries are deleted at start; sr-setup's `SeleneRetrieval/*` purge is unchanged. **R41's release recheck still applies** (reworded in 04-roadmap). Details: `docs/research/m5-c-cli-providers.md`. |
| 10.3 | Codex (sign in) reuses the local Codex CLI login (**owner decision 2026-10-19, M5-C; overrides Codex's `pending_vendor_approval`**) | "Codex (sign in)" is enabled the same way, for personal / private builds: status from `codex login status` (plain text; no JSON form in 0.160); calls through `codex exec --json --ephemeral --skip-git-repo-check --ignore-user-config --ignore-rules --sandbox read-only -C <empty temp folder> [-m <model>] -` (prompt on stdin) with every tool-like feature off (`-c features.<name>=false`, incl. `shell_tool`, `apps`, `plugins`, `hooks`, `browser_use`, `computer_use`, `view_image`, `image_generation`, `multi_agent`, `memories`), no MCP servers, no web search, no project docs; **never** an approval or sandbox bypass flag. Before the first call the effective feature state is read back (`codex … features list`) and the call is refused unless `shell_tool` is off (fail closed). Not installed → "Codex isn't installed on this computer" + https://developers.openai.com/codex/cli; not signed in → [Sign in] opens `codex login` in its own window. Selene's Codex route (CLIProxyAPI sidecar reusing the Codex client id and its tokens) is **not** copied; `~/.codex/auth.json` is never read. | Owner decision 2026-10-19 (personal/private builds). Release gate **R42** (new, mirrors R41): before any public or distributed build, recheck against OpenAI's terms then in force and keep a record of approval. |

### Amendment 11: M5 close-out (M5 tester, 2026-10-19)

Evidence: `research/m5-test-report.md` (checkpoints `m5-test-progress.md`). No new cargo-deny exception. Nothing outside
`projects/selene-retrieval/` was changed; repo-root `.github` and `scripts/bundle-whisper.js` untouched; no commits.

| # | Area | Decision | Evidence |
|---|---|---|---|
| 11.1 | CI (owner decision a) | M5-I's diff is **not** applied to the repo-root `.github/workflows`; the project copy keeps the sr-setup / msi-ca `cargo deny` step. The tester's gate runs `cargo deny` for all three lock files. | gate deny ×3 OK |
| 11.2 | DLL search (owner decision b) | `harden_dll_search()` keeps System32 **plus the app's install folder** (bundled ONNX Runtime / DirectML / pdfium); current directory and PATH stay excluded. | check-manifest S-08 6/6 mains |
| 11.3 | Installer toolset (owner decision d) | WiX stays on **v5.0.2** (MS-RL; no v6/v7 OSMF EULA). | dist MSI builds |
| 11.4 | Installer helper (owner decision c) | `sr-setup.exe` may request a **drained** `host.shutdown` without the trusted-peer check; nothing else (`host.restart`, `folders.add` refused, verified on release). | `logs/s05-setup.json` |
| 11.5 | Claude (sign in), R41 / R42 | The OAuth / paste-code implementation tested first (fake OAuth server, 16/16) is **superseded** by M5-C's CLI login reuse (Amendments 10.2 / 10.3). **Owner decision 2026-10-19: M5-C's R41 and R42 rows are approved as written** (they replace the OAuth-era R41 sentence; no other R41). The tester's debug hook `SR_CLAUDE_OAUTH_TOKEN_URL` was removed with the OAuth code (grep: none left). | integration notes "Owner confirmation, 2026-10-19" |
| 11.6 | Hold, lifted | After M5-C the **full** gate ran once (g3) and was green on the first run (768 / 0 / 6 twice; UI 54/54), so no fix batch or rerun was needed. Fake-CLI end to end on a release daemon 13/13; real `claude auth status --json` / `codex login status` run once each for parsing (redacted), never a prompt. The real-prompt checks (Claude 2.1.303 `--setting-sources ""` in `-p` mode; Codex login with `--ignore-user-config`) stay with the owner. | report §8 |
| 11.14 | Second dist build | Owner-approved exception to "dist once per milestone": the four installers were rebuilt once after M5-C (`--msi` first). The pre-M5-C set is kept in `target/m5-test/package-pre-m5c/` for reference only. | report §8 |
| 11.15 | Reviewed `unsafe` | `sr_providers::cli::job::imp` (Job Object + `GetSystemDirectoryW`) reviewed: handles owned and closed once, sizes and buffer bounds correct; marked reviewed in 02 §2.6, together with the stub daemon's test-only `set_var`. | report §8 |
| 11.16 | Owner-requested model refresh + third dist build, 2026-10-19 | The CLI sign-in rows offer Selene's current model lists: `cargo xtask sync-providers` also reads `lib/auth/claudecode-models.ts` / `codex-models.ts` (read-only) and writes `models: [{id, name}]` (Claude 11 ids; Codex 85, Selene's "(Legacy)" ids and the `gpt-5.1-chat-latest` alias left out). Defaults are an SR policy override in the xtask table (not Selene's stale `DEFAULT_MODELS`): Claude (sign in) `claude-sonnet-5-5` / utility `claude-haiku-4-5-20251001`; Codex (sign in) `gpt-5.5` / utility `gpt-5.4-mini-low` (GPT-6 Astra / GPT-5.6 listed, not default, until a Codex CLI that accepts them is verified). Saved choices are kept. Codex `split_model` handles none/minimal/low/medium/high/xhigh/max/ultra and never splits a listed model name such as `gpt-5.1-codex-max`. One gate (g4: fmt + one test assertion) + one rerun (g5 green), then the four installers rebuilt once more (owner-approved); the previous set is in `target/m5-test/package-pre-models/`. Found: R44 (model kept across a provider switch, pre-existing, open). | report §9 |
| 11.7 | Build environment | Since M5-I's manifests are embedded by `link /MANIFEST:EMBED`, the linker needs the Windows SDK's `mt.exe`: `spikes/extract-sandbox/msvc-env.sh` now puts the SDK bin folder on PATH (CI's VS environment already has it). | g0 abort, LNK1158 |
| 11.8 | S-04 classifier | `ERROR_NOT_SUPPORTED` (50) is "AppContainer unsupported" (matched by code; std leaves it uncategorised, so such a PC never fell back). A missing `pdfium.dll` also triggers the fallback (noted on R40). | `pool::s04_tests` |
| 11.9 | Test-only stubs | `sr-test-daemon` (never packaged) gets a no-op `tracing::warn!` (keeps sr-bridge's tiny dependency graph) and trusts its callers via the debug hook (scheduled-task launches don't pass the test env); the trust rules stay tested in sr-engine. | sr-bridge 14/14 |
| 11.10 | Installer round trips on a dev PC | Must not run while the owner's own install exists (same folder, `\SR` task, AppContainer profile, autostart values, data dir and credential prefix). NSIS / MSI round trips, uninstall-leaves-nothing and the R-UX43 fake-roots check move to the VM matrix or an owner-uninstalled PC; the MSI-over-NSIS refusal (read-only) was run. | `m5-test-progress.md` |
| 11.11 | Tester rule (incident) | Live scripts must never pick "the only `sr-` pipe": they ignore pipes that existed before the run and accept a pipe only when the handshake's `daemon_pid` is the daemon they started. The M4 helper did not, and briefly reached the owner's running M4 daemon (3 temp folders added, one drained shutdown; reverted by removing exactly those folders). | `logs/owner-undo.txt` |
| 11.12 | Quality gate | selene-src **0.929** with the M5 lane gate (M4 0.917) and CQADupStack 0.363 accepted; the gate stays "no lower than the last milestone beyond noise". | `target/m5-test/ev/out` |
| 11.13 | Cadence record | One gate (g1, after an aborted toolchain run and one run contaminated by a leftover gate process, both discarded), one fix batch, two clippy-only passes, one rerun (g2) + one targeted test-only confirmation; release once (gate) + release eval tools; dist once. After M5-C: one full gate (g3, green) with its release build and one dist rebuild (11.14). | `m5-test-progress.md` |

**P2 check.**
- [x] Average user without documentation: policy locks say "Managed by your organisation"; R34 offers one safe choice with focus; "Port in use" and "reduced protection" are one line each. CLI sign-in: R-UX44 / R-UX46 (M5-C).
- [x] Zero configuration: first run 0 network endpoints; everything new is off or Advanced.
- [x] Defaults: HTTP off; policy can only restrict; reduced protection only when the container can't run.
- [x] Plain errors with one action: R34 gap closed; #40 / #41 have one action (or none for #40, by design).
- [x] Security unchanged and invisible: S-01..S-11 in code and tested (S-05 / S-03 / S-10 / S-11 live); open: second-account tests (R38 / R39), R40 low-IL token (M6), external review before GA.

### Amendment 12: Output efficiency of the MCP tools (owner: "go with recommendations", 2026-10-19)

Plan and record: `06-output-efficiency-plan.md` (§7 decisions, §11 implementation, §12 scope additions).

| # | Decision |
|---|---|
| 12.1 | New result shapes are the default; no versioned format. `path` / `line_start` / `line_end` keep their names (the eval `mcp:` adapter is unchanged); `path` is folder-relative. |
| 12.2 | Answer citation numbers keep the original `n` (no renumbering). |
| 12.3 | A merged source shows the cited range (`lines: [...]` for several); the full range only with `sources: "all"` or `debug`. |
| 12.4 | `score` / `scores` (and `count`, `took_ms`, provider / model / rounds, `from_read`) only with `debug: true`. |
| 12.5 | Three location fields kept; no `path:a-b` string in the structured rows. |
| 12.6 | A `folders` table (`root`, `name`, `id`) in every result that has rows; per-row `f` only with more than one folder; `read_file` advertises `folderIds`. |
| 12.7 | Identical text collapses in the core (app-RPC too), behind `collapse_duplicates` (default on), before top-k; `also_at` stays within the caller's folder scope. |
| 12.8 | `patches/` / `spikes/` rules live in this repo's `.seleneignore`, not in the global defaults: `spikes/*/patches/` and `eval/baselines/` excluded. **Revisited 2026-10-19 (orchestrator for the owner):** `patches/lancedb-0.40.0/**` excluded, its directories re-included, only the patched `src/job.rs` re-included (06 §11.3). |
| 12.9 | Global default: `.json` / `.geojson` with lines averaging over 1,000 bytes are skipped as `data_file`; `*.jsonl` / `*.ndjson` ignored. |
| 12.10 | A `.seleneignore` `!pattern` can re-include built-in defaults, never the sandbox or the secret denylist. |
| 12.11 | Watcher events honour `.gitignore` / `.ignore` / `.seleneignore`; an edited ignore file rescans the folder. |
| 12.12 | Issue 1: `maxSources` is the hard total of passages the model receives (the first search takes three quarters; reads and deeper rounds share the rest). |
| 12.13 | Issue 2: catalog hydration is batched (500 ids per statement, text joined in) and capped (`hydrate_max_ids` 4,096); the cap shows in search stats and MCP `debug` only. `current_chunk_ids` removed (not used on any search path). |
| 12.14 | Issue 3: a stale-candidate test (50 % / 90 %, lookup counter, cap regression) instead of a heavier benchmark. |
| 12.15 | Issue 4: ignore-rule changes remove and restore indexed files through the normal sweep, triggered by ignore-file events and a built-in defaults version stamp (one rescan per folder at start). |

R-UX48 (P2: `.seleneignore` needs documentation) is in `04-roadmap.md`.

### Amendment 13: "Create a folder for me" (owner idea, 2026-10-19; orchestrated, approved to proceed)

Plan and evidence: `docs/07-default-folder-plan.md`.

| # | Topic | Decision |
|---|---|---|
| 13.1 | First-run step 1 | Two choices: **[Create a folder for me]** (primary, recommended) and **[Choose an existing folder…]** (today's flow, unchanged). Without the offer (policy, Documents not found) step 1 is exactly as before. |
| 13.2 | The folder | `Selene Retrieval` in Documents, resolved with `SHGetKnownFolderPath(FOLDERID_Documents)` through Tauri's path resolver (`dirs-sys`, already in the tree; no new dependency); follows OneDrive / folder redirection. A file or link with that name → `Selene Retrieval (2)` … `(20)`. Never `create_dir_all`, never wipes, renames or follows a link. |
| 13.3 | One click | The app creates the folder (if missing), adds it with the **existing** `folders.add` (indexed, watched), opens it in File Explorer with the safe opener, and step 2 reads "Your folder is ready" with [Open folder]; the wizard continues (provider, connect apps, Start with Windows) and **Finish opens that folder's details screen**. Closing the window at any point lands on Overview (a folder exists). |
| 13.4 | Security | **No new RPC; R31 privileged set unchanged.** The path is computed only in the app (the UI sends no path; new commands `get_default_folder`, `create_default_folder`, `open_folder(folder_id)`), and the service validates it like any picked folder (canonical root, dangerous roots, policy roots, overlap, 5 s network probe). The **app** creates the folder as the user (refused when the app runs elevated); the background service still never writes into user folders. Policy `folders.allowed_roots` managed → offer hidden and the command refuses (fail closed). `open_folder` only opens an added folder (re-checked after canonicalisation). |
| 13.5 | Elsewhere | Folders page: empty state offers both; the header shows a ghost [Create a folder for me] only while that folder isn't added or covered by an added parent. Every folder's details screen gets [Open folder]. |
| 13.6 | Failures | Access denied (incl. ransomware protection), disk full, Documents missing, Documents not answering in 5 s, elevated app, policy: plain text, one action, always the fallback [Choose a folder]. An empty folder we just created is removed again if the add fails. |
| 13.7 | Uninstall | Unchanged and verified by code reading: uninstall never touches Documents; the folder and its files are the user's data (VM-matrix round trip item). |
| 13.8 | Risks | R-UX49 (Controlled Folder Access), R-UX50 (OneDrive sync / English-only name), R45 (a new folder is watched up to 2 s after the add; the app does one catch-up check 3 s after creating the default folder). Standing check passes. |

### Amendment 14: Broad questions under the strict passage budget, `grep` shape, skipped files (owner feedback 2026-10-20; orchestrated, approved to proceed)

Plan, evidence and results: `docs/08-broad-query-ranking.md`.

| # | Topic | Decision |
|---|---|---|
| 14.1 | Diagnosis | The owner's hypothesis holds in substance: before docs/06 issue 1 the first search took all 20 passages and lance.rs was #19 of them; with the ¾ split it takes 15 and lance.rs was cut. Correction: a `read_file` step can't add a file that isn't already a source (S-09), so it never "rescued" lance.rs; deeper rounds (off by default) can. Root cause: the first stage used the fused passage order as is, so one vendored file, two test files and eval scripts filled the budget. |
| 14.2 | Answer passage selection | `sr_core::query::passages`: an answer's first search (and each deeper-round search) looks 3 × budget wide (30–150) and picks its passages greedily by fused score + a small file bonus (0.1 × second + 0.05 × third best passage of the file) − 0.003 per passage already taken from that file; tests / fixtures / benches / examples count half unless the question asks about them; a lookup (one identifier, path, file name or number) keeps the fused order. No setting; `search`, the app and `enhance_prompt` are unchanged; `maxSources` keeps its meaning and hard total (12.12). |
| 14.3 | Eval gate for ranking changes | The first design (bonus ½ / ¼, strict one-per-file) improved the broad question most but cost CQADupStack 1.9 nDCG points and selene-src 4.2 at chunk level, so it was scaled back before the gate (08 §8.1). Shipped numbers at answer-sized budgets are within noise on selene-src, its exact set and CQADupStack; the new broad set rises +14.8 nDCG@10 points. New golden set `eval/golden/selene-retrieval.broad.jsonl` (6 broad questions about this repo, the exact question first); `sr-bench-engine --first-stage answer` evaluates exactly what an answer receives. |
| 14.4 | `grep` | The docs/06 shape: `folders` once, folder-relative rows (`path`, `line`, `text`), `f` only with several folders, `truncated` only when true; `files_searched`, `abs_path` and `folder_id` only with the new `debug`. The app-RPC DTO is unchanged. |
| 14.5 | Skipped files | Text the tokenizer reads as a handful of tokens became one > 1 MiB chunk that the catalog refuses (`InvalidInput`, "That request wasn't valid", a Retry that always fails). The chunker now cuts such pieces at 1 MiB on character boundaries (`CHUNK_MAX_BYTES` = the catalog limit; every chunk storable before is unchanged, so `CHUNKER_VERSION` stays). The 9 remaining unreadable files are deliberately broken parser fixtures; they stay visible. |
| 14.6 | Repo rules / open choice | This repo's `.seleneignore` adds `spikes/*/results/` (machine-written dumps) and `spikes/*/stlshim/` (vendored Microsoft STL). No built-in "results" rule: not clearly safe for users' own results folders. Open for the owner: lance.rs is 10th on this repo at the default budget (in from `maxSources` 13); a 0.004 penalty with the bonus on a file's first pick only would make it 8th for 1.7 more chunk-level points on selene-src (08 §8.1). |
| 14.7 | Gate, installers | g1 → one fix batch (clippy `manual_clamp`, rustfmt) → g2 all green: 832 / 0 / 7 tests, sr-setup 40, msi-ca 3, UI 61, deny ×3, crt, delayload, bridge deps, manifest (6), providers. Release once (g2); `dist` once, the four installers (unsigned): NSIS 124.0 MB (sha256 19485f05…6ba6938), NSIS offline 339.2 MB (27d0655b…74d6cd0eb2), MSI 140.1 MB (01ec797b…42392997), MSI offline 352.6 MB (a568d95f…765a4eb); check-manifest (6), check-crt (4), check-delayload OK on the staged exes. Full hashes: `target/package/SHA256SUMS*.txt`. |

**P2 check.** Can an average user do this without documentation? Yes: nothing to set; answers to broad questions draw on more of the right files; `grep` output is smaller for agents; two files that showed "Couldn't be read" with a Retry that never helped now index. No new risk to the standing check; R46 logs the tuning's dependence on the RRF scale.

### Amendment 15: Standalone repository (owner request, 2026-11-06)

| # | Topic | Decision |
|---|---|---|
| 15.1 | Location | The project left `selene/projects/selene-retrieval/`, where it was never committed, for its own folder next to the Selene checkout and its own repository `tercumantanumut/selene-retrieval`. Selene is unchanged and stays a separate repository. |
| 15.2 | CI | Replaces 5.1: `.github/workflows/ci.yml` is the active workflow and runs from the repository root; the copy at Selene's root was removed. `sync-providers --check` reads Selene (public) from a sparse read-only checkout in `_selene/`. GitHub Actions starts switched off on the new repository: the workflow has never run on GitHub, and on a private repository every run uses the owner's Windows runner minutes. Turning it on is the owner's call. |
| 15.3 | Selene sources | `cargo xtask sync-providers` and the eval scripts default to a Selene checkout next to this repository (`../selene`); `--selene <dir>` (eval `.mjs` scripts: `SELENE_ROOT`) points elsewhere. |
| 15.4 | Repository hygiene | `.gitattributes` keeps LF in the repository and in every checkout (`rustfmt.toml` sets `newline_style = "Unix"`; Windows checkouts default to `core.autocrlf = true`) and marks binary types. Not committed: the generated extract fixtures (`python spikes/extract-sandbox/gen_fixtures.py --extra` rebuilds them) and `*.log`. `msvc-env.sh` finds its paths relative to itself. |
| 15.5 | Moving an indexed folder | Found while moving: the daemon holds every folder root open through cap-std (`SandboxFs`), and cap-std opens directories without `FILE_SHARE_DELETE`, so an indexed folder can't be renamed, moved or deleted while the service runs ("in use" in File Explorer). The move went through removing the folder, moving it, and adding it again. Open for the owner: a fix (drop and reopen the root handles around a folder change, or detect the rename and follow it) is not scheduled. |

### Amendment 16: MCP tool surface (owner, 2026-11-08)

Owner's words: "remove grep tool, not necessary. its not our job. read file tool not our job. add folder, sync folder only gets to agents if user activates those features. otherwise manually. no need for get_status as well. enhance_prompt also removed from agents tools, its not agents job, its another pipeline. rename answer to context_answer, and search to context_search".

| # | Topic | Decision |
|---|---|---|
| 16.1 | Tools | Every session lists `context_search` (was `search`) and `list_folders`. Opt-in: `sync_folder` (new switch, below), `add_folder` (unchanged, off by default) and `context_answer` (was `answer`; listed as before when a provider is connected and answers are on). Arguments, results and progress of the renamed tools are unchanged. |
| 16.2 | Removed from MCP | `read_file`, `grep`, `get_status` and `enhance_prompt`. The engine features stay where SR uses them: `read_file` in the answer pipeline and app-RPC `files.read`, `grep` in app-RPC `files.grep`, status in the app and the `sr://status` / `sr://folders` resources, the prompt enhancer on the app's Enhance page. Deleted: the MCP-only parameter types, definitions and result formatters of the four tools, and `ToolFlags.enhance`. |
| 16.3 | No aliases | Clean break before release: an old or removed name gets the same `-32602` "unknown tool" answer as any unknown name. Hidden opt-in tools: 16.6. |
| 16.4 | `sync_folder` switch | `[integrations] mcp_sync_folder_enabled` (default `false`, applies at once, `tools/list_changed` when it flips). Settings → Advanced: "Let connected apps check folders for changes", next to "Let connected apps ask to add folders". Folders keep updating on their own (except a folder set to check only when the user clicks), and "Check for changes now" stays in the app. Policy: `[integrations] allow_mcp_sync_folder` / registry `AllowMcpSyncFolder` (restrict only; locked switch shows "Managed by your organisation"). |
| 16.5 | Text | Server instructions name `context_search`, and `context_answer` when available; tool descriptions and results point to no removed tool. The "Files only" note now says connected apps don't see what's in those files (they can no longer open them). Open for the owner: whether the "Only list the files" mode is still worth keeping (closed: removed, Amendment 17). |
| 16.6 | A hidden tool answers like an unknown one | While `sync_folder`, `add_folder` or `context_answer` is not listed, a call to it gets exactly the answer of any unknown name (JSON-RPC `-32602` "unknown tool <name>"), as the MCP spec does for tools a server doesn't list; the old `ApprovalDenied` text ("You declined … or it expired") was untrue for a switched-off feature. The MCP layer checks the same flags as `tools/list` before dispatching, and each tool checks its switch again once the engine has started, so the list and the call never disagree; nothing runs. The engine's own checks stay (`request_add_folder` still refuses with `ApprovalDenied` if the switch went off meanwhile). No exception: `context_answer` follows the rule too, because its old "No AI provider set up" result was wrong when a provider is connected and only answers are off. |

**P2 check.** Can an average user do this without documentation? Yes: nothing to set; AI apps see fewer, clearer tools; the new switch is off by default, explained in one line and never needed, because folders update on their own by default and the app has "Check for changes now". No new risk to the standing check.

### Amendment 17: The Files-only folder mode is removed (owner, 2026-11-08)

Owner decision: remove the "Only list the files" mode. It let AI apps open and grep a folder's files without SR reading them; since Amendment 16 removed `read_file` and `grep` from MCP, it only gave AI apps the folder's name and file count.

| # | Topic | Decision |
|---|---|---|
| 17.1 | Removed | The per-folder Advanced choice "Search the contents, or only list the files" (`FolderSettings.indexing_mode`, now 15 fields), its confirm, and the "Files only" note and badge. An app or MCP request, a hand-edited `settings.toml` or a policy that still names `indexing_mode` loads without an error (the key is ignored; no policy key ever locked it). Closes the open point in 16.5. |
| 17.2 | Existing folders | A folder still set to Files only is never read without the user's say-so. Catalog migration V5 **pauses** it (the existing pause: no scan, no watcher, its jobs held) and keeps `folders.indexing_mode = 'files_only'` as a mark (no longer a setting). Its files stay listed. Its card and detail page say "Paused until you choose what to do" and "This folder was set to only list its files, which is no longer possible. Search its contents, or remove it from Selene Retrieval.", with [Search its contents] in place of [Resume] and the usual [Remove…]. Such folders sort first. |
| 17.3 | The two answers | [Search its contents] is `folders.resume`: it clears the mark with the pause and reads every file, whatever the folder's sync mode (a `settings_change` scan). [Remove…] is the usual remove flow. Nothing else unpauses it: start-up, "Check for changes now", MCP `sync_folder` and settings changes leave a paused folder unread. |
| 17.4 | Kept | The `files_only` skip reason and the "listed" file count (never "skipped"): access-grant folders are listed and never read (`reconcile::behavior_of`, the claim's no-op for them), and the migrated folders' files stay listed until the user chooses. The `indexing_mode` column and its V1 CHECK (no DDL). Removed: `IndexingMode` (common and app), `ChangeImpact::FilesOnly`, the vector → files-only switch in `Reconciler::update_settings` (it deleted vectors), the files-only check in the engine's `index_file`, and `files_only` on `FolderInfo`, `FolderStatusSummary` and `FolderView` (replaced by `was_files_only`). |
| 17.5 | M6 importer | Not built. The rule of 17.2 applies to it: a Selene folder in `files_only` mode is not imported as a searched folder without the user's say-so. |

**P2 check.** Can an average user do this without documentation? Yes: one Advanced choice fewer, and a folder that was set to only list its files explains itself in one plain sentence on its card, with both answers as buttons; nothing in it is read until the user picks one. No new risk to the standing check.

### Amendment 18: macOS port (owner, 2026-11-14)

Owner decision: build the app on macOS. Asked to choose between a phased port with the same security, a developer-only build without the parser sandbox, a compile-only port, or no change, the owner chose the phased full port, then: "go ahead throughout all phases end to end".

| # | Topic | Decision |
|---|---|---|
| 18.1 | Platforms | Windows 10/11 x64 stays the primary platform; macOS on Apple silicon (aarch64-apple-darwin) is added. Windows behaviour does not change: every Windows code path stays and is gated `cfg(windows)`. |
| 18.2 | Same security | Every Windows protection gets a macOS equivalent or fails closed: named pipe + user-only DACL → Unix socket 0600 in a 0700 per-user folder under the Darwin per-user temp dir, peer uid checked on both sides, extended ACLs removed; `icacls` data dir → owner-only 0700 without ACL entries; Credential Manager → login Keychain; AppContainer + Job Object for `sr-extract` → Seatbelt deny-default profile, resource limits and process-group kill. Until the parser sandbox exists the worker never starts on macOS. |
| 18.3 | Fail-closed choices | The socket folder is refused when the per-user temp folder carries an ACL allow entry (another account could replace our folder); the service then does not start rather than run with a shared socket folder. |
| 18.4 | Phases | 1: compiles on macOS. 2: runs end to end (POSIX path sandbox, macOS dangerous roots, hardware tier on Apple silicon, volume identity, FSEvents, audit-token peer trust). 3: parser sandbox. 4: `.app` / `.dmg` packaging. Status and risks (R49–R52, R-UX53–R-UX54) in `04-roadmap.md`. |
| 18.5 | Distribution | A macOS build is distributed only after phase 3, and signed and notarized only once a Developer ID exists (R-UX54); until then it is ad-hoc signed for development. |

**P2 check.** Can an average user do this without documentation? Windows users see no change. On macOS the target is the same: one `.dmg`, drag to Applications, the wizard. Until signing exists Gatekeeper blocks the first start (R-UX54); that is logged, not hidden.
