# M4 app-RPC and event contract (engine side)

> Owner: M4-E (engine, MCP server, client, catalog, sync, common). Written 2026-10-19, **before** the engine
> work, so that M4-A (`crates/app`, `ui/`) can code against it in parallel. **Stable from now on:** any later
> change is appended under "Changes" at the end, with a date, and never edited in place.
>
> Binding inputs: Product Principles P1/P2 (`../product-principles.md`), Amendments 1–8 (esp. 7 and 8), 02 §2.2/§2.4/§2.5/§2.6, page briefs 03, 16, 18,
> 24–32.

## 0. Conventions (unchanged from M1–M3)

- **Transport.** app-RPC = JSON-RPC 2.0 over the named pipe after a `kind=app` handshake, length-prefixed frames
  (`sr_common::protocol::rpc`). Calls run concurrently; replies can arrive out of order.
- **Errors.** Every failure is the one error model: `RpcError { code: -32000 | -32602, message, data: { code:
  ErrorCode, retryable, retry_after_s?, ctx? } }`. The app renders it through the one renderer (page 15).
  **M4 adds no error code** (the table stays at 37). Bad or missing params → `InvalidInput` (`-32602`).
- **Events.** Push only: `events.subscribe` → `events.event` notifications carrying `Seq<EngineEvent>`. New M4
  variants use dotted wire names (the `type` tag), like `tier.detected`. Nothing in this contract needs a timer
  on the app side; "refetch after an event" is push-driven, not polling.
- **Types.** Every new DTO and method name lives in **`sr_common::api`** (new module; the app already depends
  on `sr-common`). New ids: `sr_common::ids::GroupId` (ULID string, like `FolderId`). Times are **Unix
  milliseconds (`i64`)**. Sizes are bytes (`u64`).
- **Privileged methods (R31, §11).** Marked **[P]**. They answer only a pipe session whose peer process is our
  own `sr-app.exe` (or the `sr-daemon admin` CLI) next to the daemon. Anyone else gets `PermissionDenied`.
  The app needs no change for this; it only must be the installed `sr-app.exe`.

## 1. Summary of what M4 adds

| Area | Methods | Events | Page |
|---|---|---|---|
| Service | `host.restart` | `host_state` (existing) | 03 |
| Folder groups | `groups.list` `groups.create` `groups.rename` `groups.delete` `groups.move_folder` | `groups.changed` | 31, 25 |
| Connected apps | `clients.list` `clients.set_scope`[P] `clients.forget` `clients.snippet` | `clients.changed` | 29 |
| HTTP (opt-in) | `http.status` `http.set_enabled`[P] `http.create_token`[P] `http.rotate_token`[P] `http.revoke` | `http.state` | 29 |
| Folder detail | `folders.detail` `folders.skipped` `folders.retry_files` `folders.update_settings` | (existing folder events) | 25 |
| Search / Ask / Enhance | `search` (unchanged), `answer` (+`request_id`), `enhance_prompt` (+`request_id`), `requests.cancel` | `request.progress` | 26, 27, 28 |
| Activity | `activity.list` `activity.egress` `activity.note` | `activity.added` `egress.recorded` | 30 |
| Index location | `index_location.status` `index_location.check` `index_location.move`[P] `index_location.cancel` | `index.move.progress` `index.move.done` `index.move.failed` | 32 |
| Existing, now [P] | `consent.set` `approvals.approve` `providers.save` | — | 21, 22, 14 |

Snapshot (`engine.status` / subscribe snapshot) additions: `EngineStatus.index_move`, and per folder
`group_id` and `stopped` (also on `folders.list` rows).

## 2. Service: restart (page 03, R-UX31, OQ-15/OQ-24)

### `host.restart` → `{ "restarting": true }`
- Params: `{}`. Served by the pipe server itself (like `host.shutdown`). Not privileged (any same-user process can
  already end the daemon).
- Effect: the daemon replies, pushes `host_state {state: "draining"}`, drains (finishes the current batch,
  ≤ 10 s target, Amendment 4.25) and exits.
- **App side:** keep the subscription; when the pipe closes, re-attach at once (`sr_client::attach` launches a
  fresh daemon through `sr-launch` / the scheduled task). Show "Restarting the background service…" from the
  `draining` event until the new run's `host_state {ready}`. The new run's events have another `stream`, so the
  app re-snapshots (existing behaviour).
- The same flow runs after `index_location.move` (§9).

## 3. Folder groups (page 31; Advanced only)

**Design (engine decision, see `m4-e-engine.md` Deviations):** a folder group is a **label over folders**
(catalog table `folder_groups`, `folders.group_id`). All groups share the one search index and the one search
model (the active index of the default store collection, Amendment 1). Moving a folder between groups never
re-reads or re-embeds anything; it only changes which apps may see it. "Search model" on a group card is the
global model (from `search_model.status`); there is **no per-group model change in M4** (page 31's [Change
search model] should open Settings → Search).

The default group is named **"My folders"** (`is_default: true`), always exists, can be renamed, can't be
deleted. New folders (app add, approved `add_folder`) go to the default group.

```rust
pub struct GroupView {           // sr_common::api::GroupView
    pub group_id: GroupId,
    pub name: String,
    pub is_default: bool,
    pub folder_ids: Vec<FolderId>,  // folders in this group (removing folders excluded)
    pub created_at: i64,
}
```

| Method | Params | Result | Errors |
|---|---|---|---|
| `groups.list` | `{}` | `{ "groups": [GroupView] }` (default first, then by name) | — |
| `groups.create` | `{ "name": string }` | `GroupView` | `InvalidInput` (empty, > 80 chars, or the name is taken, case-insensitive). The UI should check "name taken" locally against `groups.list` to show its own field text |
| `groups.rename` | `{ "group_id", "name" }` | `GroupView` | `NotFound`, `InvalidInput` (as create) |
| `groups.delete` | `{ "group_id" }` | `{ "moved_folders": u32, "clients_without_access": [ClientView] }` | `NotFound`; `InvalidInput` for the default group |
| `groups.move_folder` | `{ "folder_id", "group_id" }` | `{ "folder_id", "group_id" }` | `NotFound` (folder or group) |

- `groups.delete` moves the group's folders to the default group first (nothing leaves search), then removes the
  group from every client scope. Scopes **fail closed**: an app limited to the deleted group does **not** widen to
  "All my folders". `clients_without_access` lists the apps that now see no folder at all, so the app can show
  page 31's notice "{app} no longer has access to any folders…" with [Open Connected apps].
- Event **`groups.changed {}`** after any create / rename / delete / move (refetch `groups.list`; folder rows also
  carry `group_id`).

## 4. Connected apps and per-client scopes (page 29)

A **client** is one connected app: a stdio app seen over the pipe (`--client <id>` of `sr-bridge`, normalised to
lowercase: `claude`, `cursor`, `vscode`, anything else = `other`), or one HTTP tool (one token each). Rows are
created automatically the first time an app connects (and by `http.create_token`).

```rust
pub enum ClientScope {               // serde: {"mode":"all"} | {"mode":"groups","group_ids":[...]}
    All,                             // default (decision 6): "All my folders"
    Groups { group_ids: Vec<GroupId> },
}
pub struct ClientView {
    pub client_id: String,           // "claude", "cursor", "vscode", "<other id>", "http-<ulid>"
    pub kind: ClientKind,            // claude_desktop | cursor | vs_code | other | http
    pub display_name: String,        // "Claude Desktop", "Cursor", "VS Code", the id, or the HTTP tool's name
    pub scope: ClientScope,
    pub first_seen_at: Option<i64>,
    pub last_seen_at: Option<i64>,   // "Last used {time}"; updated at most once a minute per app
    pub sessions_open: u32,          // live pipe/HTTP sessions right now
    pub token_last4: Option<String>, // HTTP only: last 4 chars, for "hidden, ends in {last4}"
}
pub struct Snippet { pub kind: ClientKind, pub file_hint: String, pub text: String }
```

| Method | Params | Result | Errors |
|---|---|---|---|
| `clients.list` | `{}` | `{ "clients": [ClientView], "http": HttpStatus }` (by kind, then name) | — |
| `clients.set_scope` **[P]** | `{ "client_id", "scope": ClientScope }` | `ClientView` | `NotFound`; `InvalidInput` (unknown group id) |
| `clients.forget` | `{ "client_id" }` | `{ "removed": bool }` | — (an HTTP client's token is revoked too) |
| `clients.snippet` | `{ "kind": ClientKind, "client_id"?: string }` | `Snippet` | `InvalidInput` (http without `client_id`) |

- **Scope rules (MCP side, pipe and HTTP).** Every MCP tool and resource only sees the client's scope: `search`,
  `list_folders`, `get_status` (folder lines), `sync_folder`, `read_file`, `grep`, `answer`, `enhance_prompt`,
  `sr://status`, `sr://folders`. A request naming folders outside the scope is refused like a path outside the
  folders (`PathOutsideSandbox` / `NotFound`); a client whose scope resolves to no folder gets an empty result
  with an agent sentence: "This app doesn't have access to any folders in Selene Retrieval. Ask the user to
  choose its folders in Selene Retrieval → Connected apps." `add_folder` requests are unaffected (the user
  approves each one).
- A scope change applies to the **next tool call** (no reconnect).
- Note for the UI copy (page 29): for apps on this computer the scope is a privacy setting, not a security
  boundary (a local app could read the files anyway); for HTTP tools it is enforced per token.
- `clients.snippet` text (OQ-44, the server key is `selene-retrieval`, the command is the installed
  `sr-bridge.exe` by absolute path; no token is ever in a stdio snippet):
  - `claude_desktop` / `other`: `{"mcpServers":{"selene-retrieval":{"command":"…\\sr-bridge.exe","args":["--client","claude"|"other"]}}}`,
    `file_hint` "claude_desktop_config.json" / "your app's MCP settings";
  - `cursor`: same `mcpServers` shape with `--client cursor`, `file_hint` "%USERPROFILE%\\.cursor\\mcp.json";
  - `vs_code`: `{"servers":{"selene-retrieval":{"type":"stdio","command":"…","args":["--client","vscode"]}}}`,
    `file_hint` ".vscode\\mcp.json or the user mcp.json";
  - `http` (needs `client_id`): `{"mcpServers":{"selene-retrieval":{"type":"http","url":"http://127.0.0.1:<port>/mcp","headers":{"Authorization":"Bearer <token>"}}}}`
    with the literal placeholder `<token>`; the real token is only in `http.create_token` / `http.rotate_token`
    replies.
- Event **`clients.changed { "client_id": string | null }`**: a client appeared, its scope / name / token changed,
  it was forgotten, or its `last_seen_at` / `sessions_open` moved (throttled: at most once a minute per client
  for `last_seen_at`; session open/close is pushed at once). Refetch `clients.list`.
- The existing `client_connected {client}` event stays as is.

## 5. Streamable HTTP (opt-in, off by default; page 29 Advanced)

Security is fixed and invisible (P1.7): binds **127.0.0.1 only** (never another address; there is no setting for
it), a **per-tool 256-bit bearer token** (43-char base64url), stored only in Windows Credential Manager as
`SeleneRetrieval/http-token:<client_id>` (the catalog holds no secret; the daemon keeps a BLAKE3 hash in memory),
**Host** must be `127.0.0.1:<port>` or `localhost:<port>`, an **Origin** header, if present, must be
`http://127.0.0.1:<port>` or `http://localhost:<port>` (DNS-rebinding guard), request bodies ≤ 4 MiB, at most 16
concurrent requests, 30 s per non-streaming request. Endpoint: `http://127.0.0.1:<port>/mcp` (MCP streamable HTTP,
rmcp). The port is `[integrations] http_port`; if empty, a free port is picked once and saved.

```rust
pub struct HttpStatus {
    pub enabled: bool,               // [integrations] http_server_enabled
    pub running: bool,
    pub port: Option<u16>,
    pub url: Option<String>,         // "http://127.0.0.1:<port>/mcp" while running
    pub error: Option<ErrorCode>,    // e.g. Internal when the port could not be opened
    pub tools: u32,                  // HTTP clients with a token
}
pub struct TokenCreated { pub client: ClientView, pub token: String, pub snippet: Snippet }
```

| Method | Params | Result | Errors |
|---|---|---|---|
| `http.status` | `{}` | `HttpStatus` | — |
| `http.set_enabled` **[P]** | `{ "enabled": bool }` | `HttpStatus` | `Internal` (port couldn't open; setting is still saved) |
| `http.create_token` **[P]** | `{ "name": string }` (1–60 chars) | `TokenCreated` | `InvalidInput`; `Internal` (Credential Manager) |
| `http.rotate_token` **[P]** | `{ "client_id" }` | `TokenCreated` (old token stops at once) | `NotFound` |
| `http.revoke` | `{ "client_id" }` | `{ "revoked": bool }` (token deleted; the tool's next request is refused) | — |

- **Applies at once:** turning HTTP on starts the listener without a restart; off stops it and ends open HTTP
  sessions. (This supersedes `x-restart = true` on the two `[integrations]` HTTP fields; page 29's "Turns on after
  Selene Retrieval restarts" state is not needed.)
- The token is returned **once** (create / rotate). The UI shows it masked with [Copy] and "Keep it private"
  (OQ-45); to copy it again later, rotate it. Tokens never appear in logs, diagnostics or events.
- Event **`http.state` = `HttpStatus`** on every start / stop / failure / token count change.
- HTTP clients have `kind: http` and their own scope (default "All my folders", same as stdio apps).

## 6. Folder detail (page 25, OQ-37/38/39)

```rust
pub struct FolderDetail {
    pub folder: FolderInfo,          // existing shape (+ group_id, stopped; see §10)
    pub settings: FolderSettings,    // sr_common::config::FolderSettings (labels = its doc comments)
    pub group_id: GroupId,
    pub group_name: String,
    pub paused: bool,
    pub last_run: LastRun,
    pub counts: FileCounts,
    pub skipped: Vec<SkippedFile>,   // first 50, newest first
    pub skipped_total: u64,
}
pub struct LastRun {
    pub trigger: Option<String>,     // startup | watcher | schedule | manual | mcp | settings_change | reconnect
    pub last_full_scan_at: Option<i64>,
    pub last_success_at: Option<i64>, // "Last checked {time}"
    pub partial: bool,               // the last full scan couldn't open some subfolders
    pub stopped: bool,               // the user pressed Stop and no full scan finished since (OQ-37 "stopped")
}
pub struct FileCounts { pub indexed: u64, pub pending: u64, pub skipped: u64, pub listed: u64 }
pub enum SkipReason { TooLarge, OnlineOnly, Unreadable, Timeout, Permission, Locked, Other }
pub struct SkippedFile {
    pub file_id: FileId,
    pub rel_path: String,
    pub reason: SkipReason,          // page 25 reasons table
    pub code: Option<ErrorCode>,     // FileTooLarge | FileOnlineOnly | FileUnreadable | ExtractTimeout | PermissionDenied | FileLocked
    pub retryable: bool,             // [Retry] shown when true
    pub size: u64,
    pub updated_at: i64,
}
```

| Method | Params | Result | Errors |
|---|---|---|---|
| `folders.detail` | `{ "folder_id" }` | `FolderDetail` | `NotFound` (page 25 "isn't in Selene Retrieval anymore") |
| `folders.skipped` | `{ "folder_id", "offset"?: u32 = 0, "limit"?: u32 = 50 (≤ 500) }` | `{ "total": u64, "files": [SkippedFile] }` | `NotFound` |
| `folders.retry_files` | `{ "folder_id", "file_ids"?: [FileId] }` (absent = every retryable file) | `{ "queued": u32 }` | `NotFound` |
| `folders.update_settings` | `{ "folder_id", "settings": FolderSettings }` (the whole object, as read from `folders.detail`) | `{ "reindex": bool, "detail": FolderDetail }` | `NotFound`, `InvalidInput` (bounds), `Conflict` (folder busy being removed) |

- `files_only` listings are **never** in `skipped` (they are `counts.listed`). Secret-denylisted files never enter
  the catalog (page 25 assumption holds).
- `reindex: true` means the change re-reads the folder (file types, files-only, chunking, size limit…); the UI
  confirms **before** calling (page 25 "This re-reads the folder…"). Switching to "Only list the files" removes
  the folder's search data (existing sr-sync behaviour).
- Stop / Continue: existing `folders.cancel` / `folders.sync`; Pause / Resume: existing `folders.pause` /
  `folders.resume`.

## 7. Search, Ask and Prompt Enhancer (pages 26–28)

### `search` (unchanged)
Params `SearchArgs { query, max_results?, min_score?, folder_ids?, mode? }` → `SearchOutput { hits[], total_ms,
dropped_stale, no_folders, no_good_match, top_dense, top_bm25 }`; hit = `{ path, rel_path, folder_id,
folder_name, line_start, line_end, snippet (≤ 4000 chars, plain text), score, scores { rrf, dense, bm25, rerank } }`.
Hits are chunk-level in rank order; the UI groups them by file (page 26). Snippets are untrusted text: render as
text, never markup. "Show more results" = the same call with a larger `max_results` (≤ 150).

### `answer` (changed: progress, cancel)
- Params: `AnswerArgs { question, folder_ids?, max_sources? }` **+ `request_id?: string`** (client-chosen, 1–64
  chars of `[A-Za-z0-9_-]`, unique per call; a ULID is fine).
- Result (unchanged): `AnswerOutput { text, citations[{source, path, line_start, line_end}], sources[{n, path,
  rel_path, folder_name, line_start, line_end, snippet, from_read}], no_good_match, no_folders, provider, model,
  rounds, tool_steps, files_read, structured, input_tokens, output_tokens }`. `citations[].source` = the `n` of a
  `sources` row; every citation is validated against the shown sources (Amendment 7.7).
- While running, with a `request_id`, the engine pushes **`request.progress`** events:
  `{ "request_id", "kind": "answer", "phase": "searching" | "reading_files" | "writing" | "searching_again",
  "round"?: u32, "max"?: u32 }` (page 27 lines "Reading your files…", "Writing the answer…", "Searching again
  (round {n} of {max})…").
- **Streaming text (OQ-42): not in M4.** The answer protocol is a JSON reply (Amendment 7.7), so the text arrives
  complete with the result; the UI shows the phase line meanwhile.
- **Cancel:** `requests.cancel { request_id }` → `{ "cancelled": bool }`; the `answer` call then fails with
  `Cancelled` and the provider request is dropped (nothing more is billed). Closing the app's pipe session also
  cancels its in-flight `answer` / `enhance_prompt` calls.

### `enhance_prompt` (changed: cancel)
- Params: `EnhanceArgs { prompt, memories?, history?, attachments?, agent?, folder_ids? }` **+ `request_id?`**.
  The page sends `prompt` and optionally `folder_ids` only (OQ-43).
- Result (unchanged): `EnhanceOutput { enhanced, prompt, original, used_llm, fallback, input_type, files[],
  chunks_retrieved, skip_reason?, fallback_code?, input_tokens?, output_tokens?, provider?, model? }`. `fallback`
  → "Improved without AI this time ({provider} didn't respond)."; `files.len()` → "Used context from {n} files".
- Progress: one `request.progress {request_id, kind: "enhance", phase: "searching" | "improving"}` per phase.
- Cancel as for `answer`.

### `requests.cancel`
`{ "request_id" }` → `{ "cancelled": bool }` (`false` if unknown or already finished).

## 8. Activity (page 30, OQ-47)

```rust
pub enum ActivityKind {   // snake_case on the wire
    FolderAdded, FolderRemoved, FolderSynced, FilesSkipped, SearchUpgraded,
    ClientConnected, ClientDisconnected, ClientFirstSeen,
    ApprovalAllowed, ApprovalDeclined, ApprovalExpired,
    Updated, IndexMoved,
}
pub struct ActivityEntry {
    pub id: i64,                     // paging key, newest = largest
    pub ts: i64,
    pub kind: ActivityKind,
    pub folder_id: Option<FolderId>,
    pub folder_name: Option<String>, // name at the time (kept after removal)
    pub client_kind: Option<ClientKind>,
    pub client_name: Option<String>,
    pub count: Option<u64>,          // FolderSynced: files; FilesSkipped: files
    pub text: Option<String>,        // SearchUpgraded: "Better" / "Best" / "Basic" / "Multilingual"; Updated: version; IndexMoved: new path; Approval*: the folder path
}
pub struct EgressRow {
    pub id: i64,
    pub ts: i64,
    pub provider: String,            // provider id
    pub provider_name: String,       // display name
    pub purpose: Purpose,            // UI words via Purpose::plain()
    pub bytes: u64,
    pub client_id: Option<String>,   // None = this app
    pub client_name: String,         // "This app" | "Claude Desktop" | …
}
```

| Method | Params | Result |
|---|---|---|
| `activity.list` | `{ "before_id"?: i64, "limit"?: u32 = 50 (≤ 200) }` | `{ "entries": [ActivityEntry], "next_before_id": i64 \| null }` (newest first; last 7 days) |
| `activity.egress` | `{ "before_id"?: i64, "limit"?: u32 = 50 (≤ 200), "provider"?: string }` | `{ "rows": [EgressRow], "next_before_id": i64 \| null, "providers": [ { "id", "name" } ] }` (`providers` = every provider ever audited; empty ⇒ hide the card) |
| `activity.note` | `{ "kind": "client_connected" \| "client_disconnected", "client_kind": ClientKind }` | `{ "id": i64 }` |

- Sources: engine hooks write one row per event into the catalog table `activity_log` (7-day retention; pruned by
  the maintenance pass): folder added / removed (app, approved request), a full check or first index that ends
  synced (`FolderSynced`, trigger ≠ watcher), a run that ends with skipped files (`FilesSkipped`), `index.swapped`
  (`SearchUpgraded`), approvals decided or expired, a new daemon build at start (`Updated`), a finished index move
  (`IndexMoved`), a new client's first session (`ClientFirstSeen`). Claude/Cursor/VS Code Connect and Disconnect
  happen in the app (config writers), so the app records them with `activity.note`.
- Egress rows come straight from `egress_audit` (kept as long as it is; no retention in M4). Query and file text
  are never stored or shown (only purpose and size).
- Events: **`activity.added { "entry": ActivityEntry }`** and **`egress.recorded { "row": EgressRow }`** (prepend
  while the page is open).

## 9. Move the search index (page 32, OQ-49)

What moves: the **search index** = `catalog.db` (+ `-wal`/`-shm`), `lance\`, `tantivy\` and downloaded `models\`.
What stays in `%LOCALAPPDATA%\SeleneRetrieval`: `settings.toml`, `logs\`, the lock files and a small pointer file
`index-location.json` (so the pipe name, scheduled task and every client config stay valid). The index goes to
`<chosen folder>\SeleneRetrieval Index` (created by us; the chosen folder itself is never written to otherwise).

```rust
pub struct IndexLocationStatus {
    pub path: String,                // current index folder
    pub is_default: bool,
    pub size_bytes: u64,
    pub free_bytes: u64,             // on its drive
    pub moving: Option<IndexMoveProgress>,
}
pub enum MoveProblem {   // snake_case
    NotLocal,        // network / UNC / removable / not a drive letter → "Choose a folder on a drive in this computer, with a short path."
    TooLong,         // over the path rule (3.12) → same sentence
    InsideFolder,    // inside (or containing) an added folder → "This folder is one of your searched folders. Choose another place."
    InsideApp,       // inside the install dir or the current index
    NotEmpty,        // "<chosen>\SeleneRetrieval Index" exists and is not empty
    NotWritable,     // couldn't create a test file there
    SameAsCurrent,
}
pub struct IndexMoveCheck {
    pub ok: bool,
    pub problem: Option<MoveProblem>,
    pub index_path: String,          // "<target>\SeleneRetrieval Index"
    pub drive: String,               // "D:"
    pub need_bytes: u64,             // size × 1.1 + 512 MiB
    pub free_bytes: u64,
    pub enough_space: bool,          // false → "Not enough space on {drive}. About {need} is needed."
}
pub enum MovePhase { Waiting, Copying, Verifying, Switching, Cleaning }
pub struct IndexMoveProgress { pub phase: MovePhase, pub done_bytes: u64, pub total_bytes: u64, pub from: String, pub to: String }
```

| Method | Params | Result | Errors |
|---|---|---|---|
| `index_location.status` | `{}` | `IndexLocationStatus` | — |
| `index_location.check` | `{ "target": string }` (the folder the user picked) | `IndexMoveCheck` | `InvalidInput` (empty / relative) |
| `index_location.move` **[P]** | `{ "target": string }` | `{ "started": true, "index_path": string }` | `InvalidInput` (check failed), `DiskFull` (31, `ctx.need`/`ctx.drive`), `Conflict` (a move is already pending) |
| `index_location.cancel` | `{}` | `{ "cancelled": bool }` (only during Waiting/Copying/Verifying) | — |

Flow (app side):
1. `index_location.check` live after the picker; enable [Move] when `ok && enough_space`.
2. `index_location.move` → the reply, then `host_state {draining}`: the daemon saves the plan, drains and exits.
   The app re-attaches exactly as after `host.restart` (§2).
3. The new daemon run opens the pipe **before** the engine (as today) and performs the move first: events
   **`index.move.progress` = `IndexMoveProgress`** (≤ 4 Hz), the snapshot's `EngineStatus.index_move` carries the
   same progress, and the summary reads "Moving the search index (n%)". Search and other engine calls wait or
   answer `Busy` ("still starting") meanwhile: page 32's "Search is paused while it moves".
4. End: **`index.move.done { "path" }`** then the normal start (`host_state {ready}`), or **`index.move.failed {
   "code": ErrorCode, "kept_path": string }`** (31 DiskFull, 8 PermissionDenied, 36 Internal, 18 Cancelled) and
   the start continues on the old location: "Nothing was moved. Your index is still in {old path}."
- Safety: copy → verify (size + BLAKE3 of every file) → one atomic pointer write (temp + rename) → start on the
  new location → delete the old copy only after the new start succeeded. A crash at any point leaves a working
  index at one of the two places; an interrupted copy resumes at the next start (verified files are skipped) or
  is dropped by `index_location.cancel`. The new folder gets the protected ACL (user + SYSTEM) **before** any
  file is copied (4.14); if that fails, the move fails with nothing changed.
- `settings.toml [system] data_dir` is updated to the new path for display only; the pointer file is the truth.

## 10. Changed shapes (additive; old fields unchanged)

- `FolderInfo` (`folders.list`, MCP `list_folders`) and `FolderStatusSummary` (snapshot): **`group_id: GroupId`**
  (always set; the default group for older rows) and **`stopped: bool`** (OQ-37; omitted when false).
- `EngineStatus`: **`index_move: Option<IndexMoveProgress>`** (omitted when none). `summary` while moving:
  "Moving the search index (n%)".
- `SearchOutput`: **`no_access: bool`** (omitted when false; only MCP clients with an empty scope ever see it).
- `AdvancedSettings` (`settings.advanced`): unchanged; HTTP lives in `http.*`.

## 11. R31 mitigation: privileged methods need our own app

- At `kind=app` handshake the daemon reads the peer process id from the kernel (`GetNamedPipeClientProcessId`)
  and its image path (`QueryFullProcessImageNameW`). The session is **trusted** when that path is
  `<daemon dir>\sr-app.exe` or `<daemon dir>\sr-daemon.exe` (the hidden admin CLI), compared case-insensitively
  after normalisation. Authenticode of the peer is added once signing exists (M5).
- Privileged methods: `consent.set`, `approvals.approve`, `providers.save`, `clients.set_scope`,
  `http.set_enabled`, `http.create_token`, `http.rotate_token`, `index_location.move`. An untrusted session gets
  `PermissionDenied` (logged with the peer's file name only). Everything that only narrows access (deny, revoke,
  forget, consent via MCP) stays open.
- Debug builds only: `SR_TRUST_ANY_APP=1` trusts every same-user app session (tests). Ignored in release, like
  every dev hook.
- Residual (documented in `m4-e-engine.md`): a same-user process can still inject into `sr-app.exe` or replace
  files in the per-user install dir; the check raises the bar from "speak the handshake" to "tamper with our
  process or install", which is the best a same-user boundary allows.

## 12. Method name constants

`sr_common::api::methods::*` holds every name above (`HOST_RESTART = "host.restart"`, `GROUPS_LIST =
"groups.list"`, …) and `sr_common::api::PRIVILEGED: &[&str]`. Event wire names are the `type` tags of the new
`EngineEvent` variants: `groups.changed`, `clients.changed`, `http.state`, `request.progress`, `activity.added`,
`egress.recorded`, `index.move.progress`, `index.move.done`, `index.move.failed`.

## Changes

### 2026-10-19 (M4-E, during implementation; all additive or clarifying, no shape removed)

1. **Snapshot `tier.multilingual: bool`** (`TierSummary`, omitted when false): the active index uses the
   multilingual model. `tier.active` still reads `minimum` for it (its resource class, M3-Q deviation 6), so the
   app's `tier_of_model` / page 12 / About can say "Multilingual" from this flag.
2. **Activity:** `approval_expired` is defined but **not emitted in M4** (no expiry hook yet). `folder_synced` is
   written for a folder's **first** index and for checks started by the user or an app (`manual`, `mcp`) or by a
   settings change, **not** for the hourly schedule, the start-up check or saved files (they would add a line per
   folder per hour). `files_skipped` follows the same rule.
3. **`ClientView.sessions_open`** counts pipe sessions. HTTP tools are request-based: their `sessions_open` is 0;
   use `last_seen_at` ("Last used").
4. **`HttpStatus.tools`** counts every HTTP tool, also while HTTP is off.
5. **`index_location.status` while a move runs** (the engine isn't started yet) answers from the move's progress:
   `path` = the old location, `size_bytes` = bytes to copy, `free_bytes` = 0, `moving` = the progress.
6. **Refused privileged calls** (R31) carry `PermissionDenied`; its table text ("No permission to read…") only
   ever reaches a caller that isn't our app, never the user.
7. **Provider `key_url`** (page 21 "Where do I get a key?", Amendment 8.3) is **not** in the manifest yet: the
   manifest is generated by `xtask sync-providers` from its `POLICY` table (outside M4-E's crates). The app
   already hides the link when the field is missing. Follow-up for the orchestrator / xtask owner.
8. **`files_only: bool`** on `FolderInfo` (`folders.list`, MCP `list_folders`) and `FolderStatusSummary`
   (snapshot), omitted when false: the folder is listed, not searched by content ("Only list the files", or an
   access grant). Page 24's "Files only" badge (M4-A request R-E2).
9. **2026-10-19, output efficiency (docs/06, additive).** `search` (`SearchHitOut`): `folder_root` (display
   form of the folder root) and `also_at[]` (`{folder_id, folder_name, folder_root, rel_path, line_start,
   line_end}`, omitted when empty: other places with exactly this text; identical chunks are collapsed into
   one hit before `max_results`, on by default). `SearchArgs.collapse_duplicates` (optional, `null` = on).
   `SearchOutput.hydrate_capped` (omitted when false: the stale-hit lookup cap stopped the search early).
   `answer` (`SourceOut`): `folder_id`, `folder_root`, `also_at[]`. `maxSources` is now the **total** number of
   passages the answer uses (first search, `read_file` excerpts and deeper rounds), not only the first search.
   `folders.skipped` reason `data_file` ("Looks like a data file (very long lines), not a document", not
   retryable). The app ignores the new fields.

### 2026-11-08 (Amendment 17: the Files-only folder mode is removed)

1. **`FolderSettings.indexing_mode`** is gone (`folders.detail`, `folders.update_settings`). A request that still
   sends it is accepted and the key is ignored. `folders.update_settings` no longer has a files-only switch, so it
   never removes a folder's search data.
2. **`files_only: bool`** (Changes 2026-10-19, item 8) is replaced by **`was_files_only: bool`** on `FolderInfo`
   and `FolderStatusSummary`, omitted when false: the folder was set to only list its files, so it stays paused
   until the user chooses. `folders.resume` on such a folder clears the flag and reads every file, whatever the
   folder's sync mode (`scan_queued: true`). `folders.remove` is unchanged.
3. **`counts.listed`** stays: files listed, not read (an access grant's files, and those of a folder paused by
   Amendment 17 until the user chooses).
