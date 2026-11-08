# 06. Output efficiency of the MCP tools (`answer`, `search`)

Status: Approved 2026-10-19 (owner: go with recommendations; spikes sub-question → only spikes/*/patches/)

> Implemented 2026-10-19 (see §11 "Implementation record"). Decision 8 was revisited the same day: `patches/lancedb-0.40.0` is no longer kept fully searchable (§11.3). Scope additions: issues 1–4 (§12).
> Binding: Product Principles P1 (average-user simplicity, never weakening security) and P2 (standing check), see `product-principles.md`. Amendments in `00-review-and-decisions.md` stay binding.

Scope: the owner's five changes only. (1) Cited-only sources, (2) folder path once, (3) omit empty, default and debug fields, (4) collapse duplicates at query time, (5) index-time noise exclusion. Out of scope: `maxResults` defaults, the `maxSources` limit, ranking quality, compact or table output modes, and everything else. Items found outside that scope are listed under "Follow-ups" and are not part of this plan.

## 1. Summary

- **Measured, not guessed.** I built a release `sr-daemon` and ran it against its own data dir and pipe. It indexed a copy of this repo (52,585 chunks). `answer` ran against a local OpenAI-compatible stub, and nothing was sent to a real model. I captured the full `CallToolResult` for 5 `answer` and 7 `search` calls. Each change was then simulated by post-processing the captured JSON (tokens: `tiktoken` `o200k_base`). Change 5 is not a simulation: I re-indexed with the rules applied and ran the calls again.
- **The owner's 4–5k → under 1k estimate holds for the reported case.** A deeper answer (40 sources, 10 cited) goes from **5,500 tokens to 900** with changes 1–3, and to **872** with all five (−84 %). A default answer (deeper answers off, 12–13 sources) goes from **~2,100 to 580–670** (−69 to −72 %), and to 490–615 with all five.
- **`search` is the larger cost and shrinks less.** A default `search` (50 hits) is **~28,000 tokens**, and changes 2+3 cut **~28 %**. Snippet text is ~13.5k of the ~22k structured tokens, and the five changes don't touch it. Changes 4 and 5 mostly improve *what* fills the 50 slots, not how many tokens they take: in the lancedb query, 24 of 50 hits were duplicates and 24 were noise, covering only 12 distinct files.
- **Noise is most of the index.** `eval/baselines/**` was **48.6 %** of all chunks, all of `spikes/` was 23.8 % and `patches/` 11.4 %. The project's own `crates/` was 12.4 %. With `eval/baselines/` and `spikes/*/patches/` excluded, the index fell from 52,585 to 21,031 chunks. The existing sweep purged them in 3.2 s, and they were hidden from search at once.

## 2. Findings

### 2.1 Where the shapes are built and serialized

| What | Where | Notes |
|---|---|---|
| Tool definitions (schemas) | `crates/mcp-server/src/tools.rs:224-305` (`search` 226-242, `read_file` 268-284), `answer_definition` `tools.rs:328-345` | `answer` takes `question`, `folderIds`, `maxSources` only. `read_file`'s schema lacks `folderIds`, but `ReadFileParams` accepts it (`tools.rs:124-139`). |
| `answer` structured | `tools.rs:479-493` `answer_structured` | Emits `text`, `citations` (as is), `sources[] {n, path, rel_path, folder, line_start, line_end, from_read}`, `no_good_match`, `no_folders`, `provider`, `model`, `rounds`. Drops `snippet`, `tool_steps`, `files_read`, `structured` and the token counts. |
| `answer` text | `tools.rs:496-523` `answer_text` | Answer text, then "Sources:" with **cited sources only**, each as an absolute path, then "Written by {provider}…". |
| `search` structured | `tools.rs:548-566` `search_structured` | Per hit: `path` (absolute), `rel_path`, `folder_id`, `folder`, `line_start`, `line_end`, `score`, `scores{rrf,dense,bm25,rerank}`, `snippet` (≤ 4,000 chars, `handle.rs:111`). Envelope: `count`, `no_good_match`, `no_access`, `took_ms`. |
| `search` text | `tools.rs:569-605` `search_text` | Absolute path, line range and the first 300 chars of the snippet per hit. |
| Envelope | `crates/mcp-server/src/server.rs:55-59` `ok_structured` | **Every result carries both** `content[0].text` and `structuredContent`. The text is a subset of the structured data. |
| Calls | `server.rs:204-236` (`search`), `server.rs:400-466` (`answer`) | Scope is applied before the engine (`scope.restrict`, `server.rs:224`, `:420`; `crates/engine/src/clients.rs:67-87`). |
| Errors | `server.rs:49-53` `tool_error`; `crates/common/src/error/mod.rs:353-359` | `isError: true` + `{code, retryable, retry_after}`. Not touched by this plan. |
| Engine DTOs | `crates/engine/src/answers.rs:51-81` (`SourceOut`, `AnswerOutput`), `crates/engine/src/handle.rs:57-98` (`SearchHitOut`, `SearchOutput`), hit mapping `handle.rs:970-997` | These are also the **app-RPC** shapes (contract `research/m4-rpc-contract.md` §7). |

### 2.2 How sources and citations are assembled (`crates/core/src/answer.rs`)

- The first search (`engine/src/answers.rs:266-295`, `max_sources` default 12) gives the sources. They are numbered 1.. by `renumber` (`answer.rs:674-687`), which drops exact (path, lines) duplicates only and caps them at `ANSWER_MAX_SOURCES = 40` (`answer.rs:54`).
- **`from_read`** (`answer.rs:77-79`): `true` only for an excerpt the model asked for with a `read_file` step. The excerpt becomes a new numbered, citable source (`answer.rs:576-595`; up to 20,000 chars, often a wide range such as 1–120). `merge_sources` (deeper rounds) forces it to `false` (`core/src/iterate.rs:44`), and `passages_answer` skips read sources (`answer.rs:634-643`). It **is** true in practice: 1 source in A1 and 1 in D1. Its only signal for an agent is "this range came from a model read, not from the search". That is debug-level information.
- `validate_citations` (`answer.rs:410-446`) keeps only citations of sources the model was shown, clamps their lines inside the source, adds bare `[n]` markers that had no explicit citation, and orders by first mention. A citation's range is usually **narrower** than its source's range (for example source 1–17, citation 5–9).
- **40 sources happen only with deeper answers on.** The default is 12 sources plus ≤ 6 reads (max 36 even with `maxSources: 30`). Deeper rounds add ≤ 8 hits per query (`answers.rs:157-188`) up to 40 (`iterate.rs:32-50`). Every source (cited or not) is serialized twice in the structured part when cited (`sources` + `citations`, both with the absolute `path`), and a third time in the text.
- The **app already shows cited sources only, once, with the original `n`** (`crates/app/src/search.rs:196-260`, `answer_view`). It reads the app-RPC `AnswerOutput`, not MCP.

### 2.3 `text_hash` at query time

- Schema: `chunk_text(text_hash PK, zstd, bytes, refcount)` and `chunks(… text_hash …)` with `ix_chunks_text_hash` (`crates/catalog/src/schema.rs:198-226`). Lance rows also carry `text_hash` (`crates/core/src/index/lance.rs:3-5`, `VectorRow` `lance.rs:128-136`). This matches docs/00 C3.
- `chunk_id = blake3(file_id ‖ content_hash ‖ cfg ‖ ord)` and `text_hash = blake3(NFC(text))` (`core/src/ingest/chunk.rs:12-13`, `:306`; test `common/src/ids.rs:227`). So identical text in two files gives **different chunk ids and equal text hashes**, consistent with roadmap M1 ("Chunk IDs unique across duplicate files").
- **Almost free at query time.** The catalog hydrate query already selects `c.text_hash` (`crates/catalog/src/repo/chunks.rs:304`, read at `:318`) to fetch the text, then discards it: `Hydrated` (`chunks.rs:287-297`), `HydratedChunk` (`core/src/index/source.rs:61-72`, adapter `engine/src/adapters.rs:263-293`) and `SearchHit` (`core/src/query/mod.rs:106-118`) have no `text_hash` field. Plumbing it through costs **no extra SQL**.

### 2.4 Indexing, ignore rules and purge

- Walker: `crates/sync/src/walker.rs:150-176` (`ignore` crate). It honours `.gitignore`, `.git/info/exclude` and `.ignore` inside the folder (not parents, not the global gitignore). `PathFilter` (`crates/sync/src/filter.rs`) runs in `filter_entry`: sandbox components + secret denylist (never off), Selene default dirs/files/globs (`filter.rs:27-104`; `vendor`, `target`, `dist`, `node_modules`, `*.lock`, …), user `exclude_globs`, then the include set. `.json` is indexed (`DocKind::Code`).
- **Gap (from code reading, not reproduced):** the watcher event path (`crates/sync/src/reconcile.rs:1146-1260`) checks `PathFilter` only, not `.gitignore`/`.ignore`. A saved file in a git-ignored directory gets indexed by the event and swept again by the next full scan. Editing an ignore file doesn't trigger a scan either; its effect waits for the next reconcile (60 min default).
- **Purge on rule change works already:** `finish_scan` (`crates/catalog/src/repo/files.rs:348-405`) deletes unseen files whose nearest scanned ancestor directory was listed completely, and an ignored subdirectory's parent counts. Each delete writes its outbox op in the same transaction. Chunks leave the catalog at once, so hydration hides them immediately (`core/src/query/mod.rs:509-525`). **Measured:** with an `.ignore` file, `sync --wait` removed 31,554 chunks in 3.2 s, and the next search showed none of them. Settings changes to `exclude_globs` already bump `generation` and re-read (`catalog/src/repo/folders.rs:275-292`, `app/src/folders.rs:215-240`).
- `policy.toml` has no exclude rules (`crates/common/src/config/policy.rs:83-99`; restrict-only switches and roots).

### 2.5 Consumers of the MCP shapes

| Consumer | Reads | Affected by |
|---|---|---|
| Agents (Claude, Cursor, VS Code, HTTP tools) | `content[].text` and/or `structuredContent` | All five; LLMs adapt to any shape. |
| xtask eval `mcp:` adapter | `structuredContent.hits[].path`, `line_start`/`line_end` (serde-required `path`) (`xtask/src/eval/adapters.rs:354-378`, `golden.rs:38-46`) | 2 and 3 **only if field names change**. A folder-relative `path` is fine: `normalize_path` (`golden.rs:62`) strips prefixes anyway, and golden paths are relative. |
| `crates/mcp-server/tests/daemon.rs:260-297` | `hits[0].path` ends with `corpus\src\billing.rs`; `count == 0`; `no_good_match == true` | 2 (absolute path) and 3 (`count`). |
| `tools.rs` unit test `:661-679` | `count`, `no_good_match` | 3. |
| App / UI (`crates/app/src/search.rs`, `ui/src/pages/Search.tsx`) | **app-RPC** `SearchOutput` / `AnswerOutput`, not MCP | Only 4 (if done in the core: an additive `also_at`, fewer duplicate rows). |
| Selene (M6) | Nothing yet. Only a CI workflow mentions the project; M6 is optional and not started. | None today. |
| `eval/scripts/*` | No MCP shapes | None. |

### 2.6 Owner assumptions checked

| Assumption | Verdict |
|---|---|
| ~4–5k → < 1k tokens | **True for the reported case** (deeper answer, 40 sources): 5,500 → 900. A default answer is ~2,100, not 4–5k; it goes to ~600. |
| 40 sources, 10 cited | Reproduced exactly (D1: 40 / 10). Only with **deeper answers on**. |
| Cited sources appear twice | True in `structuredContent`, and a third time in the text. |
| Three location forms per source | True (`path`, `rel_path`, `folder`), and citations repeat the absolute `path`. In D1 the absolute prefix appears 60 times (40 sources + 10 citations + 10 text lines). |
| Noise fields | True: `from_read: false` ×39, `no_good_match`/`no_folders: false`, `provider`, `model`, `rounds`. `model` is `null` only when the provider default is used. |
| lancedb copies byte-identical | **True for the 151 shared files.** The spike copy has one extra `Cargo.lock`, which `*.lock` already excludes. In the index: 5,984 chunks each, **all 5,984 pairs share `text_hash` and line ranges, 0 share `chunk_id`.** |
| `text_hash` ≠ chunk id | True (see 2.3). |
| `eval/baselines/*.json` are "one-line" dumps | **Partly wrong.** They are one JSON result per query per line (182 or 508 lines of ~4.5–4.9 KB), and there are 12 files including `ablations/`. They were **48.6 % of all chunks** (25,570). |
| `from_read` is noise | It is debug-level, but it **does occur** (see 2.2). |
| `text_hash` is available without extra lookups | Read but discarded today. Needs plumbing, no extra query (2.3). |
| Not stated, found | `search` costs ~28k tokens per default call, ~13x an answer. The text part repeats paths and snippet prefixes already in `structuredContent`. Duplicates in the index go beyond lancedb: 9,942 duplicate chunks in 7,121 groups, 5,095 of them outside the lancedb copies (spike fixtures, baselines, provider test fixtures). |

## 3. Baseline measurements

Setup: release build, own data dir and pipe (owner's install untouched), folder = this repo's source copy (without `target/`, `installers/`, `vendor-bin/`, models, `ui/node_modules`), bundled bge-small. `answer` used provider `ollama` with `network.base_url` pointing at a local stub. The stub replies in the answer protocol with plausible `[n]` markers and sub-ranges, and does a `read_file` step in A1/D1. Deeper rounds in D1/D3: 2.

Token counts are `o200k_base`. **Text** = `content[0].text` decoded. **Structured** = `structuredContent` as compact JSON. **Sum** = what a client gives a model if it forwards both. **Raw** = the whole JSON-RPC line on the wire.

| Call | What | Raw (bytes / tok) | Text tok | Structured tok | Sum tok |
|---|---|---|---|---|---|
| A1 | answer, lancedb merge_insert (hits the duplicate), 1 read | 6,370 / 2,211 | 428 | 1,734 | 2,162 |
| A2 | answer, Selene V2 baseline params (hits `eval/baselines`) | 6,571 / 2,189 | 425 | 1,717 | 2,142 |
| A3 | answer, citation validation (clean) | 6,276 / 2,091 | 495 | 1,543 | 2,038 |
| D1 | A1 question, **deeper on** (40 sources, 10 cited, 1 read) | 16,070 / 5,557 | 714 | 4,786 | 5,500 |
| D3 | A3 question, deeper on (37 sources, 10 cited) | 13,801 / 4,561 | 645 | 3,858 | 4,503 |
| S1 | search, lancedb merge_insert (default k = 50) | 100,736 / 29,303 | 6,965 | 22,073 | 29,038 |
| S2 | search, V2 baseline params (k = 50) | 90,690 / 28,557 | 7,123 | 21,134 | 28,257 |
| S3 | search, walker / gitignore (k = 50) | 95,213 / 28,356 | 6,797 | 21,328 | 28,125 |
| S1k10 | S1 with `maxResults: 10` | 18,650 / — | 1,364 | 4,129 | 5,493 |

Composition of a k = 50 search's structured part: snippets ~13.5k tokens, location fields (`path`, `rel_path`, `folder_id`, `folder`) ~4.2–5.0k, `score` + `scores` ~2.2k.

Quality signals in the same captures:

| Call | Duplicates in result | Noise in result (`eval/baselines/`, `spikes/*/patches/`) | Distinct files |
|---|---|---|---|
| S1 (50) | **24** | **24** | 12 |
| S2 (50) | 1 | 17 | 28 |
| S3 (50) | 6 | 9 | 28 |
| A1 (13 sources) | 3 sources shown twice to the model | 3 (1 cited) | — |
| A2 (12 sources) | 0 | **10 (all 5 cited)**: the answer came from eval dumps | — |
| D1 (40 sources) | **13** | 13 (3 cited) | — |
| After change 5 (all) | 0 | 0 | S1 13, S2 23, S3 26 |

## 4. Per-change design

Shared conventions (all changes):

- One new optional argument on `answer` and `search`: `debug: boolean` (default `false`). With `debug: true` the response contains everything it contains today, plus `also_at` (change 4). Debug output is for diagnosis, not for agents.
- Location format: the structured part keeps the **field names** `path`, `line_start`, `line_end` (the eval adapter and typed clients keep working), and `path` becomes **folder-relative**. The text part uses `rel/path:10-20`. A `rel_path` never contains `:` (the sandbox rejects ADS and drive-relative components), so the string form parses unambiguously.
- Changes 1–3 live **only in the MCP serializers** (`tools.rs`). The engine DTOs and the app-RPC contract stay unchanged, so the UI is unaffected. Change 4 lives in the core and is additive on app-RPC. Change 5 lives in `sr-sync`.

### 4.1 Change 1: cited-only sources, one merged list (`answer`)

**Parameter.** `sources: "cited" | "all" | "none"`, default `"cited"`.

**Shape.** `citations` is removed. `sources` holds one entry per cited source, in order of first citation, with its **original `n`**. Each entry carries the **cited** range. When the model cited one source with several ranges, the entry carries `lines: ["10-14", "30-41"]` instead of `line_start`/`line_end`. `"all"` adds uncited sources (their source range, plus `"cited": false`). `"none"` returns `text` only (plus flags). The text part lists the same entries ("Sources:"), as it already does for cited sources.

**Numbering: keep the original `n` (recommended, Owner decision needed).** Renumbering would mean rewriting `[n]` markers inside model-written text. `markers()` (`answer.rs:384-406`) treats every `[digits]` as a marker, including code such as `arr[2]` or `[1, 3]` inside the answer, so a rewrite can corrupt the text. Gaps ([1] [3] [5]) cost nothing and agents handle them. The app already shows original numbers (`app/src/search.rs:196-260`).

Before (D1, trimmed to 2 of 40 sources and 1 of 10 citations):
```json
{"text":"… merge.rs handles part 4 of this [7]. …",
 "citations":[{"source":7,"path":"C:\\Users\\<you>\\…\\selene-retrieval\\patches\\lancedb-0.40.0\\src\\table\\merge.rs","line_start":208,"line_end":216}],
 "sources":[
  {"n":1,"path":"C:\\Users\\<you>\\…\\selene-retrieval\\crates\\core\\src\\index\\lance.rs","rel_path":"crates/core/src/index/lance.rs","folder":"selene-retrieval","line_start":1,"line_end":17,"from_read":false},
  {"n":7,"path":"C:\\Users\\<you>\\…\\selene-retrieval\\patches\\lancedb-0.40.0\\src\\table\\merge.rs","rel_path":"patches/lancedb-0.40.0/src/table/merge.rs","folder":"selene-retrieval","line_start":200,"line_end":232,"from_read":false}],
 "no_good_match":false,"no_folders":false,"provider":"Ollama","model":"stub-model","rounds":2}
```
After changes 1+2+3 (default):
```json
{"text":"… merge.rs handles part 4 of this [7]. …",
 "folders":[{"root":"C:\\Users\\<you>\\…\\selene-retrieval","name":"selene-retrieval","id":"01M5AQA1XSM1H9M41KECSCPK00"}],
 "sources":[{"n":7,"path":"patches/lancedb-0.40.0/src/table/merge.rs","line_start":208,"line_end":216}]}
```
Text part after: `…[7].\n\nSources (in C:\…\selene-retrieval):\n[7] patches/lancedb-0.40.0/src/table/merge.rs:208-216\n\nWritten by Ollama from the user's files. Check the sources.`

**Code.** `tools.rs` `AnswerParams` (`:374-403`), `answer_definition` schema (`:328-345`), `answer_structured` (`:479-493`), `answer_text` (`:496-523`). There is no engine change: `AnswerOutput` already carries `citations` and `sources`.

**Edge cases.**
- A `[n]` marker without a valid source stays plain text, as today.
- A source cited only through an explicit `citations` entry and not by a marker is still listed (`validate_citations` keeps it).
- A read source (`from_read`) that is cited is listed like any other, and in debug it is flagged.
- `no_good_match` / `no_folders` keep their plain text, and the structured part keeps the `true` flag.
- `passages_answer` fallback text cites `[n]` markers that point at sources, so those sources are listed.
- `"none"` with `debug: true`: debug wins for the debug fields only.

**Tests.**
- Unit tests in `tools.rs`: cited-only order and original `n`; several citations of one source become `lines`; `"all"` marks uncited sources; `"none"`; unknown value → `-32602`.
- `crates/mcp-server/tests` or `engine/tests/answers.rs` with the existing scripted LLM: an answer citing [1] and [3] of 5 lists exactly 2 sources.
- Snapshot of the D1-like shape.

### 4.2 Change 2: folder path once (`answer` and `search`)

**Shape.** A `folders` array in the envelope, one entry per folder that appears in the response: `{root, name, id}`. Rows carry folder-relative `path`. When the response spans **more than one** folder, each row adds `"f": <1-based index>`. With one folder (the common case) there is no per-row reference. The text part prints the root once ("Sources (in C:\…)" / "12 results in C:\…:") and `rel:a-b` per row. With several folders it prints a legend `[1] = C:\…`.

Search after changes 2+3 (one hit shown):
```json
{"folders":[{"root":"C:\\Users\\<you>\\…\\selene-retrieval","name":"selene-retrieval","id":"01M5AQA1XSM1H9M41KECSCPK00"}],
 "hits":[{"path":"crates/sync/src/walker.rs","line_start":1,"line_end":22,"snippet":"//! Reconcile walker: …"}]}
```

**Why `id` and `root` stay in the table.** A relative path is resolved by `read_file` against each folder in order, and the first match wins (`engine/src/sandboxfs.rs:150-198`). Two folders with `README.md` would make `read_file("README.md")` ambiguous. With `root` the agent can pass an absolute path (`root\` + `path`; mixed separators are normalised, `common/src/sandbox/mod.rs:522`), or pass `folderIds: [id]`. **Sub-task:** add `folderIds` to the `read_file` schema (`tools.rs:273-281`). The parameter is already accepted (`tools.rs:137-138`) but not advertised.

**Code.** `tools.rs` `search_structured`/`search_text`/`answer_structured`/`answer_text`. The folder root is needed. `SearchHitOut`/`SourceOut` only carry the absolute path and the name, so add `folder_root: String` to `SearchHitOut` (engine `handle.rs:57-72`, from `folder_map`, `handle.rs:885-898`) and to `SourceOut`/`Source` (answers path). Both are additive on app-RPC (append to the m4 contract "Changes"). Don't derive the root by string-stripping `rel_path` (long paths and `\\?\` forms make that fragile).

**Edge cases.**
- Multi-folder results are covered by `f`.
- A folder renamed between search and read: `id` is stable.
- Access-grant folders (only when named) behave the same.
- `no_access` / `no_folders` responses have no `folders`.
- Paths over 260 chars: root + rel still resolves.
- Per-client scope: only folders already in the result appear, so nothing new is revealed.

**Tests.**
- `daemon.rs:260-269` changes to `folders[0].root` + `hits[0].path == "src/billing.rs"`.
- A unit test with two folders gives `f` per row and a legend in the text.
- `read_file` with `root+path` and with `path + folderIds` resolves the same file.
- The eval adapter round trip (`xtask/src/eval/adapters.rs` tests) with a relative `path`.

### 4.3 Change 3: omit empty, default and debug fields

**Rule.** Omit `false`, `null`, empty arrays and objects. Omit debug metadata unless `debug: true`.

| Field | Today | Default after | `debug: true` |
|---|---|---|---|
| `answer.no_good_match`, `no_folders`, `search.no_good_match`, `no_access` | always | **only when `true`** (they carry signal, R-UX18, docs/02 §2.5) | always |
| `answer.provider`, `model`, `rounds` | always | omitted (the text keeps the "Written by {provider}" line: it is the user-facing trust note) | yes |
| `sources[].from_read` | always | omitted | yes |
| `search.took_ms` | always | omitted | yes |
| `search.count` | always | omitted (equals `hits.length`) | yes |
| `hits[].score`, `hits[].scores` | always | **omitted** (order is the ranking; Owner decision needed) | yes |
| `hits[].scores.rerank: null` etc. | always | omitted (nulls) | — |
| Error results (`isError`, `{code, retryable, retry_after}`) | unchanged | **unchanged** (one error model, docs/02 §2.5) | unchanged |

**Code.** `tools.rs` serializers and a `debug` field on `SearchParams`/`AnswerParams`. No engine change.

**Tests.**
- `tools.rs:661-679` asserts the absence of `count` and the presence of `no_good_match: true`.
- `daemon.rs:294` changes from `count == 0` to `hits` empty.
- New: a negative query returns exactly `{"hits":[],"no_good_match":true}`.
- `debug: true` returns today's fields.

### 4.4 Change 4: collapse identical text before top-k

**Where.** In the core searcher, so `search`, `answer` (its sources come from the same search, `engine/src/answers.rs:266-295`), deeper rounds and `enhance_prompt` all benefit. **Before truncation**, after minScore / MMR / the no-good-match cut-off and the exact-term sort: between `core/src/query/mod.rs:684-694` and `hydrated.truncate(max_results)` (`:695`).

**Plumbing (no extra SQL).**
- Add `text_hash` to catalog `Hydrated` (`catalog/src/repo/chunks.rs:287-337`, already selected), `HydratedChunk` (`core/src/index/source.rs:61-72`), the engine adapter (`engine/src/adapters.rs:263-293`) and `SearchHit` (`core/src/query/mod.rs:106-118`).
- Add `also_at: Vec<AlsoAt { folder_id, rel_path, line_start, line_end }>` to `SearchHit`, `SearchHitOut`, `sr_core::answer::Source` (`#[serde(default)]`) and `SourceOut`.
- Count distinct `text_hash` in the hydrate-window loop condition (`query/mod.rs:512`), so that collapsing never leaves fewer than `max_results` distinct hits when more exist.

**Semantics.**
- The representative is the best-ranked copy. On equal fused score, the shorter `rel_path` wins, then lexicographic order (deterministic: `patches/…` beats `spikes/native-stack/patches/…`).
- Every other copy becomes an `also_at` entry with **its own** line range. Identical text can sit at different lines in two files.
- **Partial duplicates:** two files that share only some chunks collapse only those chunk hits. Their other chunks stay separate hits. File-level grouping (UI) is unaffected.
- **Scope:** only candidates already retrieved for this request are collapsed. The lanes are filtered by the request's folders, which for MCP are the client's scope (`server.rs:224`, `clients.rs:67-87`), so `also_at` can never name a path outside the caller's scope. No catalog lookup by `text_hash` is added. If one is added later (to list *all* copies), it must filter by the same folder set. A test pins this.
- Evidence for the no-good-match cut-off is computed before collapsing (unchanged).
- `renumber` / `merge_sources` keep their (path, lines) dedupe. A later deeper-round search that returns another copy of an already-present text is dropped by `text_hash` too (add the hash check to `merge_sources`, `iterate.rs:32-50`).

**MCP rendering.** Structured: `"also_at":["spikes/native-stack/patches/lancedb-0.40.0/src/table/merge.rs:200-232"]`, using the same location format as the rows. With multiple folders each entry is prefixed with the folder index (`"2:docs/a.md:1-20"`, unambiguous because a rel path never contains `:`). Text: `   same text at: …`. On by default; `debug: true` shows it too.

**App-RPC.** Additive `also_at` on `SearchHitOut`/`SourceOut` (append to the m4 contract "Changes"). The UI ignores it until a "Also in …" line is wanted (optional follow-up). Should the app see collapsed results too? Owner decision needed. The recommendation is yes, with a `collapse_duplicates: bool` on `SearchRequest`, default `true`.

**Eval.** Collapsing can hide a golden path that only exists as a copy. Run the selene-src and CQADupStack gates once, before and after (both corpora have few exact duplicates). The `mcp:` adapter can count `also_at` as extra hits if needed.

**Tests.**
- Core unit test (HashEmbedder): two identical files give one hit with `also_at`, plus a 3rd distinct hit fills the slot.
- Partial-duplicate file.
- Scope test: a copy in an out-of-scope folder never appears.
- Deterministic representative.
- Daemon test on a small corpus with a duplicated file.

### 4.5 Change 5: index-time noise exclusion

**Mechanism.**
1. **Per-folder `.seleneignore`** (gitignore syntax, nested files allowed) through `WalkBuilder::add_custom_ignore_filename(".seleneignore")` in `walker.rs:150-176`. It has the highest precedence of the ignore files, so `!dist/docs/` can re-include something `.gitignore` excludes.
2. **Built-in noise defaults** as a gitignore-syntax ruleset with the **lowest** precedence (`WalkBuilder::add_ignore`, from a file the daemon writes into the data dir at start). A `!pattern` in a folder's `.seleneignore` therefore re-includes a default-ignored path.
3. **Never overridable:** sandbox components and the secret denylist (`PathFilter::sandbox_check`). They keep running in `filter_entry` regardless of any ignore file. Owner decision needed: whether today's hard Selene defaults (`node_modules`, `target`, `vendor`, …; `filter.rs:27-104`) also move into the overridable ruleset. The recommendation is yes, with one model, and unchanged behaviour without a `.seleneignore`.
4. **Watcher path:** build the folder's ignore matchers once per scan (the walker sees every ignore file) and cache them for `apply_events` (`reconcile.rs:1146-1260`). An event on a file named `.seleneignore`, `.gitignore` or `.ignore` enqueues a folder scan. This also closes the existing gap in 2.4.

**Proposed global defaults** (generic, safe for a non-technical user's documents folder):

| Kind | Patterns | Why safe |
|---|---|---|
| Generated / minified | `*.min.js`, `*.min.css`, `*.map`, `*.bundle.js`, `*.chunk.js`, `*.pb.go`, `*_pb2.py`, `__snapshots__/`, `*.snap` | Never hand-written content |
| Vendored | `third_party/`, `third-party/`, `vendored/` (plus today's `vendor/`, `node_modules/`, …) | Upstream copies |
| Data dumps | `*.jsonl`, `*.ndjson`, plus a **content rule**: `.json`/`.geojson` files whose average line length is > 1,000 chars are skipped as "data file" in the extract step (reason shown in the folder's skipped list; `!path` in `.seleneignore` turns the rule off for that path) | Matches `eval/baselines/*.json` (~4.5–4.9 KB lines) without touching normal JSON configs or users' CSV exports |

**`patches/` and `spikes/`: repo-only, not global (recommended, Owner decision needed).** `patches/` is a meaningful directory in many projects: patch-package and quilt patches are the user's own work. `spikes/` is this project's naming convention. A global default that silently hides a user's files breaks P1: the user can't find their file and has no documentation to tell them why. Exact duplicates (the actual harm of the two lancedb copies) are handled generically by change 4. This repo gets a checked-in `.seleneignore`:
```gitignore
# M0 spike copies of the workspace patch (exact duplicates of patches/lancedb-0.40.0)
spikes/*/patches/
# Eval result dumps (also caught by the long-line data rule)
eval/baselines/
```
Keep `patches/lancedb-0.40.0` indexed: it is the live `[patch.crates-io]` dependency (`Cargo.toml:200-201`). Whether to also exclude all of `spikes/` (23.8 % of chunks; vendored MSVC is already excluded by `vendor/`, but fixtures and spike code remain) is an owner call.

**Purge and re-index.**
- A rule change is applied by a full scan: `finish_scan` sweeps newly ignored files (`files.rs:348-405`). Catalog rows go in the same transaction as their outbox `Delete` ops, chunks are hidden from search at once (hydration), and Lance/tantivy apply the deletes from the outbox in seconds. This is well inside the 15-minute physical-removal SLA defined for folder purges (docs/00 Q7). Measured with `.ignore`: 31,554 chunks removed in 3.2 s, search clean right after.
- Re-included files are added by the same scan. Vectors are reused by `text_hash` where known.
- Shipping new defaults: store `ignore_defaults_version` in `meta`. On start, when it is older, enqueue one scan per folder (trigger = settings change). No re-embed: the chunk config fingerprint is unchanged.
- Editing `.seleneignore` → watcher event → scan (point 4). On a network share (no watcher) it applies at the next scheduled reconcile, and `sync_folder` applies it at once.

**P1/P2.** The defaults need no action. `.seleneignore` is a power-user or team file, and the existing per-folder "Exclude" setting (Advanced, `exclude_globs`) stays the in-app way. *Standing check:* "Can an average user exclude a folder without documentation?" Yes, through Folder settings. `.seleneignore` itself needs docs. Log it as **R-UX48** ("`.seleneignore` is a file-based, undocumented-in-app control; the app should show '{n} files ignored by rules in this folder' on page 25"). *Security:* a `.seleneignore` arrives with cloned content (attacker-controllable), but it can only narrow or re-include **inside** the folder. It can never override the sandbox or the secret denylist, and `max_file_bytes` still bounds a re-included `node_modules`.

**Tests.**
- `crates/sync/tests/scan.rs`: `.seleneignore` excludes; `!` re-includes a default; `!` can't re-include `.env`/`*.pem`.
- A rule edit followed by a scan sweeps the files.
- A watcher event under an ignored dir is excluded.
- An event on `.seleneignore` enqueues a scan.
- `ignore_defaults_version` bump enqueues one scan per folder.
- Long-line JSON rule with an override.
- Daemon test: search is clean right after the sweep.

## 5. Expected savings (from the captures)

Tokens = text + structured, `o200k_base`. Changes 1–4 are simulated on the baseline captures. "All five" uses the real re-indexed run (change 5) with 1–4 simulated on top.

**`answer`**

| Call | Baseline | 1 only | 2 only | 3 only | 1+2+3 | All five | Saving (all) |
|---|---|---|---|---|---|---|---|
| A1 (default, read step) | 2,162 | 1,115 | 967 | 2,074 | 578 | 540 | −75 % |
| A2 (default, eval-dump question) | 2,142 | 1,095 | 1,083 | 2,054 | 673 | 487 | −77 % |
| A3 (default, clean) | 2,038 | 1,268 | 915 | 1,950 | 615 | 615 | −70 % |
| **D1 (deeper, 40 sources: owner's case)** | **5,500** | 1,852 | 2,265 | 5,277 | **900** | **872** | **−84 %** |
| D3 (deeper, 37 sources) | 4,503 | 1,663 | 1,839 | 4,290 | 793 | 751 | −83 % |

- In D1, the text part goes from 714 to 372 tokens and the structured part from 4,786 to 500. Dropping the "Written by…" footer would save 14 more tokens. Not recommended.
- Change 4 on answers barely moves response size (cited duplicates become `also_at`). Its value is upstream: in A1, 3 of 13 sources the model saw were exact copies, and in D1 13 of 40. Those slots and that provider-input context go to distinct passages instead.
- Change 5's value is upstream too: A2's answer was built entirely from eval dumps (10 of 12 sources, 5 of 5 cited), and after change 5 none were.

**`search`**

| Call | Baseline | 3 only | 2 only | 2+3 | 2+3+4 (slots refilled) | 2+3+4 (no refill) | All five | Saving (all) |
|---|---|---|---|---|---|---|---|---|
| S1 k=50 | 29,038 | 26,529 | 23,060 | 20,551 | 23,470 | 12,146 | 20,554 | −29 % |
| S2 k=50 | 28,257 | 25,759 | 22,835 | 20,337 | 20,511 | 19,927 | 21,359 | −24 % |
| S3 k=50 | 28,125 | 25,737 | 22,658 | 20,270 | 20,568 | 18,174 | 20,342 | −28 % |
| S1 k=10 | 5,493 | 4,953 | 4,386 | 3,846 | 4,772 | 3,004 | 4,144 | −25 % |

- At a fixed `maxResults`, change 4 doesn't shrink the response: the freed slots are refilled with distinct passages (S1: 24 duplicate slots → distinct results). The "no refill" column shows the duplicate share.
- The `loc`-string variant (`"loc":"path:a-b"` instead of three fields) saves only ~300 tokens per 50 hits (1.3 %). It is not worth breaking the eval adapter.
- The rest of a search response is snippet text (out of scope here).

## 6. Order of work

| Step | Change | Crates | Depends on |
|---|---|---|---|
| A | **3 + 1** together (same serializer, `answer_structured`/`answer_text`; adds `debug`, `sources`) | `sr-mcp-server` | — |
| B | **2** (`folders` table, relative `path`, `read_file` `folderIds` in schema, `folder_root` on engine DTOs) | `sr-mcp-server`, `sr-engine` (additive) | A (merged list shape) |
| C | **4** (`text_hash` plumbing, collapse, `also_at`, `merge_sources` hash check) | `sr-catalog`, `sr-core`, `sr-engine`, `sr-mcp-server` | B for the `also_at` location format only |
| D | **5** (`.seleneignore`, low-precedence defaults, data-file rule, watcher ignore cache, version-bump rescan, this repo's `.seleneignore`) | `sr-sync`, `sr-extract`/ingest (data rule), `sr-engine` (start-up rescan) | independent: can run in parallel with B/C (other crates) |

Recommended order: A → B → C, with D in parallel. A+B alone deliver ~85 % of the `answer` saving and ~70 % of the achievable `search` saving. One phase gate at the end (CLAUDE.md build cadence), one eval run for C and D (nDCG gates unchanged).

## 7. Decisions needed (owner)

1. **Compatibility: new default, no versioned format, no opt-in phase (recommended).** Owner decision needed.
   - The only programmatic MCP consumer is the eval adapter, and it keeps working because the field names `path`, `line_start` and `line_end` stay.
   - Agents read any shape. The app/UI uses app-RPC and is untouched by 1–3.
   - Selene (M6) consumes nothing yet.
   - `debug: true` and `sources: "all"` give back the full data.
   - Update `daemon.rs`, the `tools.rs` tests, the docs/03-style comment at the top of `tools.rs`, and append to the m4 contract "Changes".
2. **Citation numbers: keep the original `n` (recommended)** over renumbering, which needs rewriting model text (see 4.1). Owner decision needed.
3. **Line range of a merged source: the cited range** (with `lines: [...]` for several), with the source range in `"all"`/debug only (recommended). Owner decision needed.
4. **Drop `score`/`scores` from `search` by default** (recommended; 2.2k tokens per 50 hits, order already carries rank). Keep them under `debug`. Owner decision needed.
5. **Location encoding: keep the three fields** (recommended) vs a `loc` string (−1.3 %, breaks the eval adapter). Owner decision needed.
6. **Folder reference:** a `folders` table always, a per-row `f` only when > 1 folder, the table carries `root`, `name`, `id` (recommended). Also advertise `folderIds` on `read_file`. Owner decision needed.
7. **Collapse in the core for app-RPC too** (recommended, `collapse_duplicates` default `true`), or MCP only. Owner decision needed.
8. **`patches/` and `spikes/`: repo `.seleneignore`, not global defaults** (recommended, reasons in 4.5). Owner decision needed. **Approved; sub-questions decided: only `spikes/*/patches/`; `patches/lancedb-0.40.0` revisited 2026-10-19 → only the patched file stays indexed (§11.3).** Sub-questions:
   - Exclude all of `spikes/` or only `spikes/*/patches/`?
   - Keep `patches/lancedb-0.40.0` indexed (recommended: it is the live dependency)?
9. **Global data-dump rule:** the long-line JSON content rule + `*.jsonl`/`*.ndjson` (recommended), or path rules only. Owner decision needed.
10. **`.seleneignore` can re-include built-in defaults** (recommended), including today's Selene dir defaults. Sandbox and secrets can never be re-included. Owner decision needed.
11. **Include the watcher ignore-file fix** (2.4 gap) in change 5 (recommended: same code, otherwise `.seleneignore` would have the same hole). Owner decision needed.

## 8. Risks

| ID | Risk | L | I | Mitigation |
|---|---|---|---|---|
| R-OE1 | An agent builds a wrong absolute path from `root` + relative `path`, or reads the same rel path in another folder | M | L | `root` and `id` in `folders`; `folderIds` on `read_file`; text shows the root once per response |
| R-OE2 | Collapsing hides a copy the user actually wanted (e.g. the copy in their own folder vs a vendored one) | L | M | `also_at` names every copy with its lines; shortest-path representative; scope-only |
| R-OE3 | A new global default hides real user content | M | H | Conservative list; content rule only for long-line JSON; `!` override; skipped-reason in the UI; eval + user check |
| R-OE4 | Clients that only read `content[].text` lose `id` information | L | L | The text keeps the root and per-row folder legend |
| R-UX48 | `.seleneignore` is file-based and needs docs (fails P2 for that one path) | M | L | Defaults need nothing; in-app Exclude stays; page 25 shows "{n} files ignored by rules" |

Add R-UX48 to `04-roadmap.md` when this plan is approved.

## 9. Follow-ups found (not in scope, not planned here)

- **Text vs structured duplication.** Every result sends the same paths, and for `search` the first 300 chars of every snippet, in both parts (~7k of ~28k tokens per search).
- **Snippet length.** Snippets dominate `search` (~13.5k tokens at k = 50).
- **`grep` and `read_file`** also print absolute paths per line. The folder-once idea applies there too.
- **`answer` "Written by {provider}" footer** (14 tokens) kept for user trust.

## 10. Method notes

- **Build and isolation.** One release build: `sr-mcp-server` (`sr-daemon`), `sr-extract`, `sr-bridge`. It needed the MSVC 14.44 env from `spikes/extract-sandbox/msvc-env.sh`, because the default 14.41 fails to link ORT (Amendment 3.2). The daemon ran with `--data-dir` in a temp folder (own pipe, HTTP off) and was driven through `sr-bridge --data-dir … --client claude`. The owner's running install (`%LOCALAPPDATA%\Selene Retrieval`) was not contacted.
- **Provider stub.** Settings: `provider = "ollama"`, `network.base_url = http://127.0.0.1:<port>/v1/`, consent rows for `ollama` in the test catalog only. Release builds ignore `SR_APP_CONTAINER_NAME`, so the extractor used the existing per-user `SeleneRetrieval.Extract` AppContainer profile (reused, not modified or deleted).
- **Clean-up.** Scripts (stub, MCP driver, simulator) lived in `%TEMP%\sr-oe-research`. The daemon was shut down and the data dir, corpus copy, app copy and captures were deleted afterwards. Nothing was added to the repo except this document.

## 11. Implementation record (2026-10-19)

All 11 decisions as recommended (Amendment 12). Order: A (3+1), B (2), C (4), D (5).

### 11.1 What changed

| Change | Where | Notes |
|---|---|---|
| 1 + 3 (answer) | `crates/mcp-server/src/tools.rs` (`answer_structured`, `answer_text`, `AnswerParams.sources` / `debug`, `SourcesMode`) | `citations` gone (debug keeps a compact copy); one `sources` entry per cited source, in order of first citation, original `n`, cited lines (`line_start`/`line_end`, or `lines: ["a-b", …]` for several ranges). `sources: "all"` adds uncited ones (`"cited": false`) and `source_lines` (the whole passage); `"none"` = text only. Flags only when `true`; `provider`/`model`/`rounds`/`from_read`/`citations` only with `debug`. The "Written by {provider}" footer stays. |
| 2 + 3 (search) | `tools.rs` (`search_structured`, `search_text`, `FolderTable`), `server.rs` | `folders: [{root, name, id}]` once per response; rows carry the folder-relative `path`, `line_start`, `line_end`, `snippet`; `f` only with > 1 folder. `count`, `took_ms`, `score`, `scores`, absolute paths only with `debug` (which also shows `dropped_stale` and `hydrate_capped`). Text: `N results (in C:\…):` then `rel:a-b` per hit; with several folders `(folders: 1 = C:\a, 2 = C:\b)` and `k:rel:a-b`. `read_file` schema advertises `folderIds`. |
| 2 (engine) | `engine/src/handle.rs` (`SearchHitOut.folder_root`, `AlsoAtOut`), `engine/src/answers.rs` (`SourceOut.folder_id` / `folder_root` / `also_at`) | Additive on app-RPC (m4 contract "Changes" 9). `folder_root` of a source comes from the catalog folder list by `folder_id` (added to `sr_core::answer::Source`), not by string-stripping. |
| 4 | `catalog/src/repo/chunks.rs` (`Hydrated.text_hash`), `core/src/index/source.rs`, `engine/src/adapters.rs`, `core/src/query/mod.rs` (`collapse_duplicates`, `AlsoAt`, `SearchRequest.collapse_duplicates` default `true`), `core/src/iterate.rs` (`merge_sources` hash check) | Collapse runs after the exact-term sort and before `truncate(max_results)`; the hydrate window counts distinct `text_hash`. Representative: best-ranked copy; copies with equal content scores (dense cosine and BM25 bit-equal, i.e. only lane order differs) or equal fused score are tied → shorter path, then lexicographic. Copies outside the request's folders are dropped (pinned by a test). |
| 5 | `sync/src/rules.rs` (new), `sync/src/filter.rs`, `sync/src/walker.rs`, `sync/src/reconcile.rs`, `sync/src/service.rs`, `engine/src/ingest.rs`, `common/src/api.rs` (`SkipReason::DataFile`), `app/src/folders.rs` (reason text), `.seleneignore` (repo root) | See 11.2. |

### 11.2 Change 5 as built

- **`.seleneignore`** is registered with the walker (`add_custom_ignore_filename`, highest precedence). A `!pattern` in it also lifts the **built-in defaults** (today's Selene dirs/files/globs plus the new noise rules) for exactly the paths it matches (`FolderRules::reincluded`); a `.gitignore` whitelist does not (unchanged behaviour without a `.seleneignore`). Sandbox components and the secret denylist run first and can never be re-included (`!.env`, `!*.pem`, `!.ssh/` tested).
- **New defaults:** dirs `third_party`, `third-party`, `vendored`, `__snapshots__`; globs `*.min.js`, `*.min.css`, `*.map`, `*.bundle.js`, `*.chunk.js`, `*.pb.go`, `*_pb2.py`, `*.snap`, `*.jsonl`, `*.ndjson`.
- **Data-file rule:** in the ingest step, a `.json`/`.geojson` whose bytes / lines > 1,000 is marked `skipped` with reason `data_file` (chunks removed, outbox `Delete`), shown as "Looks like a data file (very long lines), not a document" (not retryable). Counted in bytes, not chars (JSON dumps are ASCII). A `.seleneignore` `!path` turns it off for that file.
- **Watcher:** events are checked against the same ignore files as the walk (`FolderRules::ignored`, cached per folder, rebuilt by every scan). An event on `.seleneignore` / `.gitignore` / `.ignore` enqueues one `settings_change` scan. A `settings_change` scan also hands indexed JSON files and `data_file`-skipped files to the ingest step again (unchanged ones are only hashed), so the data rule applies in both directions.
- **Defaults version:** `IGNORE_DEFAULTS_VERSION = 1` in `meta.ignore_defaults_version`. At start, a catalog with folders and an older (or missing) stamp gets one `settings_change` scan per available folder, then the stamp. The first folder added to a new catalog writes the stamp (nothing to catch up). This lives in `SyncService::startup` (sr-sync), not sr-engine as the plan table said: that is where start-up scans are enqueued. An unavailable folder gets its normal reconnect scan later (glob rules sweep then; the JSON re-check waits for its next settings change).
- **Cargo sources (defaults version 2, 2026-11-05):** the Selene directory defaults dropped Rust code whose folder is named like build output. In this repo `crates/common/build/static_crt.rs` (included by five `build.rs` scripts) was never indexed; a crate with `src/env/` or `src/build/` modules lost them too. Cargo writes only to `target/`, so the names in `CARGO_SOURCE_DIRS` (`build`, `dist`, `out`, `target`, `coverage`, `tmp`, `temp`, `env`) stay walkable inside a package's sources: a directory that holds a `Cargo.toml`, or one the nearest `Cargo.toml` at or above it (inside the folder root) reaches through `src`, `build`, `tests`, `benches` or `examples` (`FolderRules::cargo_source`, one cached `Cargo.toml` check per directory). A `dist/`, `target/` or `coverage/` next to the manifest, a JavaScript app's `build/` inside the workspace and every other default stay ignored. The version bump rescans every folder once, so existing folders pick the files up at the next start.

### 11.3 Decision 8 revisited (orchestrator for the owner, 2026-10-19)

`patches/lancedb-0.40.0` is no longer fully searchable: its upstream files (query.rs, rrf.rs, merge.rs, remote/sql_test.rs) competed with this project's code. A diff against the pristine crate (`~/.cargo/registry/src/*/lancedb-0.40.0`) shows **one** patched file, `src/job.rs` (the registry copy additionally has `.cargo_vcs_info.json` and `Cargo.lock`). The repo `.seleneignore` therefore reads (comments in the file):

```gitignore
spikes/*/patches/
eval/baselines/
patches/lancedb-0.40.0/**
!patches/lancedb-0.40.0/**/
!patches/lancedb-0.40.0/src/job.rs
```

The contents are excluded with `/**`, not the directory, and the directories are re-included with `!…/**/`, so the gitignore parent-directory rule ("a file can't be re-included if its parent directory is excluded") doesn't block the file. Tests: `rules::tests::re_include_single_files_below_an_excluded_tree` and `scan::repo_seleneignore_keeps_only_the_patched_lancedb_file` (only `src/job.rs` of the crate is cataloged; spike copies and eval dumps are not).

### 11.4 Deviations from the plan

- **Text legend.** Several folders print `(folders: 1 = C:\a, 2 = C:\b)` and rows `2:rel:a-b` (the `also_at` string format) instead of `[1] = C:\…`: in an answer `[n]` already means a citation.
- **debug rows** keep the new relative `path` (so the eval adapter reads the same field) and add `abs_path`, `folder_id`, `folder`, `score`, `scores` (search) or `abs_path`, `folder`, `from_read` (answer). `debug` on `answer` lists every source (like `"all"`), except with `sources: "none"`.
- **`folder_root` on `Source`:** the core `Source` carries `folder_id`; the root is filled in by the engine from the folder list (plan: `folder_root` on `Source`).
- **Defaults-version rescan** lives in sr-sync's start-up (11.2).
- **No eval run** of selene-src / CQADupStack in this phase (the gate does not include it); flagged for the next eval pass.

### 11.5 Re-measurement (AFTER, 2026-10-20)

Method as in §3/§10: release `sr-daemon` from the gate, own `--data-dir` (own pipe, HTTP off), driven through `sr-bridge --data-dir … --client claude`; `answer` through provider `ollama` pointed at a local OpenAI-compatible stub (consent rows in the test catalog only); nothing sent to a real model; the owner's install not contacted. Corpus: a copy of this repo (without `target/`, `installers/`, `vendor-bin/`, models, `ui/node_modules`, the vendored MSVC), **with** the repo `.seleneignore`, so the numbers are "all five" plus the revisited decision 8 and the issue 1 total. Tokens: `o200k_base`, text + compact structured JSON.

Index: **12,815 chunks** (baseline 52,585; the plan's change-5 run 21,031). 489 files indexed; 10 JSON files skipped as `data_file` (embedding-vector dumps under `spikes/native-stack/test11/` and a provider test fixture); of `patches/lancedb-0.40.0` only `src/job.rs`; `eval/baselines/` and `spikes/*/patches/` absent.

| Call | Baseline (sum) | Projected 1+2+3 | Projected all five | **AFTER** | vs baseline |
|---|---|---|---|---|---|
| A1 (default, read step) | 2,162 | 578 | 540 | **447** (text 186 + structured 261) | −79 % |
| A2 (default) | 2,142 | 673 | 487 | **462** | −78 % |
| A3 (default) | 2,038 | 615 | 615 | **440** | −78 % |
| D1 (deeper, owner's case) | 5,500 | 900 | 872 | **509** | −91 % |
| D3 (deeper) | 4,503 | 793 | 751 | **440** | −90 % |
| S1 k=50 | 29,038 | 20,551 | 20,554 | **20,012** (5,057 + 14,955) | −31 % |
| S2 k=50 | 28,257 | 20,337 | 21,359 | **20,929** | −26 % |
| S3 k=50 | 28,125 | 20,270 | 20,342 | **20,477** | −27 % |
| S1 k=10 | 5,493 | 3,846 | 4,144 | **4,427** | −19 % |

Notes on comparability:
- The stub and the question wording were rebuilt (the research scripts were deleted), so citations differ from the baseline captures: the stub cites 5 sources by default (baseline A1–A3: 4–5) and 4–8 with deeper answers (baseline D1: 10).
- **D1 is smaller than projected mainly because of issue 1:** the model now sees at most `maxSources` (12) passages in total, so D1 had 11 sources (1 read excerpt, 1 deeper round) instead of 40. With `maxSources: 20` it had 16 (8 cited), with `maxSources: 8` exactly 8 (debug captures).
- `debug: true` on A1 returns 1,315 tokens (all 10 sources the model saw, scores-free answer diagnostics); on S-calls it restores scores and absolute paths.
- Search stays snippet-bound (out of scope, §9). No `also_at` appeared in S1–S3 because the repo `.seleneignore` already removes the lancedb copies; change 4 is covered by the tests (core, daemon) instead.

## 12. Scope additions (owner, 2026-10-19): issues 1–4

### 12.1 Issue 1: `answer` used more passages than `maxSources`

- **Validated: true.** `maxSources` only sized the first search (`engine/src/answers.rs`). Each `read_file` step appended a new numbered source (`core/src/answer.rs` `one_pass`), and deeper rounds added up to 8 per query until `ANSWER_MAX_SOURCES` = 40 (`core/src/iterate.rs` `merge_sources`). Listing only cited sources (change 1) hid them from the output, but the model still received them.
- **Plan / chosen rule: one simple hard total.** `maxSources` (default 12, 1–30) is the most passages the model receives across the first search, read excerpts and deeper rounds (`AnswerConfig::max_sources`). So that reads stay possible, the first search takes `maxSources − ⌊maxSources / 4⌋` (12 → 9, 8 → 6, ≤ 3 → all; `first_search_sources`); reads and deeper rounds share the rest. A read once the total is reached is refused to the model ("this answer already has the most passages allowed (N)") and still counts as a tool step; a deeper round then adds nothing and the loop stops (no new passages). `renumber` and `merge_sources` take the total. No separate read budget.
- **Outcome.** Tests `iterate::tests::reads_and_deeper_rounds_stay_within_max_sources` (a read round and a deeper round against a total of 3: no excerpt beyond it, every prompt shows 3 sources; with room for one, exactly one excerpt is added), `merging_respects_the_total`, `answer::tests::the_first_search_leaves_room_for_reads_and_deeper_rounds`, `renumber_drops_duplicates`. Live (11.5): `maxSources` 8 → 8 sources with a read and a deeper round (the report had 27); 20 → 16 (report: 40); default → 10–11.

### 12.2 Issue 2: stale-hit filter loop unbounded, one query per id

- **Validated: true.** `Searcher::search` doubled its hydrate window until it had `2 × want` hits or ran out of fused candidates, with no cap; `catalog::repo::chunks::hydrate` ran one `query_row` per id plus one `chunk_text` query per hit.
- **`current_chunk_ids`:** not on any search path (only `catalog/tests/catalog.rs` used it; the engine's `ChunkSource::hydrate` does the staleness filter). **Removed**; the catalog test now checks that `hydrate` returns nothing for stale ids, and keeps order across a batch boundary.
- **Outcome.** `hydrate` is batched: one `SELECT … WHERE c.chunk_id IN (…)` per 500 ids (`HYDRATE_BATCH`, far below SQLite's variable limit), the chunk row and its zstd text in the same statement (`LEFT JOIN chunk_text`), results returned in the caller's order. The query loop has an explicit cap, `QueryConfig::hydrate_max_ids` (default `HYDRATE_MAX_IDS` = 4,096 looked-up ids; a normal search looks up at most 2 × lane depth × lanes per collection, far below it). The last window is cut to the cap; when the cap stops the widening, the search returns what it has. `SearchStats` gains `hydrate_lookups`, `hydrate_windows` and `hydrate_capped`; `SearchOutput.hydrate_capped` (app-RPC, omitted when false) and MCP `debug: true` (`hydrate_capped`, `dropped_stale`) show it; the default MCP output does not (decisions 3/4).

### 12.3 Issue 3: no test or benchmark for many stale hits

- **Validated: true** (`query_drops_hits_from_stale_file_versions` uses one file; the 1M bench's fake catalog has no stale rows).
- **Outcome.** `index::tests::many_stale_candidates_cost_bounded_batched_lookups` (real Lance + tantivy, 400 files; the in-memory catalog now counts `hydrate` calls and ids): 50 % and 90 % of the files get a new catalog version that the stores haven't applied, so their chunks are stale candidates. It asserts one batched lookup per window, stats equal to what the catalog saw, lookups ≤ candidates and ≤ the cap, and no stale text in the hits; with `hydrate_max_ids = 30`: ≤ 30 lookups in ≤ 2 windows, `hydrate_capped`, fewer hits. The 1M bench was not extended (the test covers the regression; no heavy run in the gate). Numbers (debug build, 2026-10-20):

| Stale share | Time | Candidates | Ids looked up / batches | Dropped stale | Hits (of 20) | Capped |
|---|---|---|---|---|---|---|
| 50 % | 6.4 ms | 113 | 113 / 2 | 56 | 20 | no |
| 50 %, cap 30 | — | — | 30 / 1 | — | 9 | yes |
| 90 % | 4.7 ms | 112 | 112 / 2 | 103 | 7 | no |
| 90 %, cap 30 | — | — | 30 / 1 | — | 0 | yes |

Before the change, the same 90 % case cost one SQL round-trip per id (112) plus one per hit for the text in the real catalog.

### 12.4 Issue 4: indexed content must leave the index when ignore rules change

- **Validated: partly already true.** A full scan sweeps newly ignored files through `finish_scan` (catalog rows and chunks in one transaction with outbox `Delete` ops; hidden at once, the stores within the purge SLA; measured 3.2 s in §2.4), and re-included files come back as new files. **Missing** were the triggers: an edited ignore file waited for the next scheduled reconcile, and new built-in defaults never reached existing folders.
- **Outcome.** An event on `.seleneignore` / `.gitignore` / `.ignore` enqueues a `settings_change` scan (the cached rules are dropped; the scan re-reads them). The built-in defaults are versioned (`meta.ignore_defaults_version`) and rescan every folder once at start (11.2). Long-line JSON follows the data rule in both directions on such scans. Tests (`sync/tests/scan.rs`): `ignore_rule_changes_sweep_and_restore_indexed_files` (rule added, watcher event → `settings_change` scan → 2 files deleted, chunks gone, outbox `Delete` ops written; rule removed → files back with index jobs; a built-in default lifted with `!`, then the lift removed: in and out again), `new_ignore_defaults_rescan_every_folder_once`, `seleneignore_excludes_reincludes_defaults_but_never_secrets`, `repo_seleneignore_keeps_only_the_patched_lancedb_file`; `reconcile::tests::settings_change_rechecks_json_data_files`; engine `long_line_json_is_skipped_as_a_data_file_unless_re_included`.

### 12.5 Gate record (CLAUDE.md cadence)

One consolidated gate (g1, `-j 12`, MSVC 14.44 env), one fix batch (a serde field-order assertion, a wrong expectation in the new `rules` test about git's parent-directory rule, one `await_holding_lock` clippy finding, rustfmt of the changed files; plus `sr-setup` built in release before `check-manifest`), one clippy-only pass, one rerun (g2). g2: UI typecheck / lint / 61 of 61 tests / build OK; workspace build OK; tests 815 passed, 7 ignored, 1 failed (`sr-engine` `default_folder` from the concurrent Amendment 13 work: timing-dependent, it passes and fails with and without this change's watcher rules, so it is left to that work); clippy `-D warnings`, fmt `--check`, `cargo deny --locked` (×3), sr-setup and msi-ca tests, release build, `check-crt`, `check-delayload`, `check-bridge-deps`, `check-manifest` (6 exes) and `sync-providers --check` all OK. No `dist` build in this phase (the concurrent work builds the installers once for both).
