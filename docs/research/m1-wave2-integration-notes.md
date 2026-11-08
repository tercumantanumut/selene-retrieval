# M1 wave 2: integration notes (orchestrator)

Requirements collected from wave-1 reports. Every wave-2 brief must include them.

## From M1-A (catalog)
- **Engine startup order (required):** open catalog → `start_epoch` → replay each derived store from its watermark (`outbox::ops_after`) → `chunks::finalize_applied` → state `ready`. Without `finalize_applied`, a crash after a store watermark save and before T2 leaves files in `pending_index` forever.
- Run `Catalog::optimize()` every 6 h from the daemon maintenance task.
- `sr-launch` needs the shared static-CRT `build.rs` (`crates/common/build/static_crt.rs`).
- CI: add an xtask `check-crt` step that fails if bridge, extract, app or launch import a VC++ DLL.
- Open decision: where `tier_override` / `use_graphics_card` / `auto` search model live (settings.toml vs catalog `meta`). One home per setting.
- Still to do: `quick_check` at open, the `model_files` repo (M2), approvals/clients/consent repos (stubs), generation in folder status.
- Accepted deviation: hybrid CRT (static VC runtime + inbox UCRT) instead of literal `+crt-static`. Record as Amendment 4.1.

## From M1-B (extractor), from its checkpoint
- Install-dir ACL: inheritable ALL APPLICATION PACKAGES RX on the install dir. Never use per-SID or per-file package ACEs (electron#54382 behaviour). This goes in the installer (M2).
- Low-IL fallback still allows read and network (known gap). AppContainer is the default.

## From M1-C (bridge)
- Under cargo's job object, the launcher takes the scheduled-task path. The tester must run the bridge tests both under cargo and directly.

## For the M1 tester
- Workspace-wide fmt/clippy once all wave-1 agents finish (leftover diffs in `protocol/extract.rs`, the sr-extract lib, sr-bridge, sr-engine).

## From M1-D (sr-providers) — required in wave 2

- **sr-engine dependency:** `sr-providers = { workspace = true, features = ["local"] }`. The workspace entry is `default-features = false`, so sr-core gets only the traits and batcher (no ORT).
- **O1 (security, mandatory):** `sr-daemon` needs its own `build.rs` that emits `sr_providers::dml::DELAYLOAD_ARGS`, and `main()` must call `sr_providers::dml::harden_dll_search()` as its first statement. Without both, a planted `DirectML.dll` loads before `main`. M1 tester: dumpbin the daemon and check that DirectML/d3d12/dxgi appear only as delay-load imports.
- **O2 (installer, M2):** ship `DirectML.dll` 1.15.x next to the daemon. Add a minimum-version check to the loader so an older signed copy is refused.
- **O3 (M2 downloader):** check whether the Standard/High fp16 GPU files output f16. fastembed only reads f32, so they would always fall back to CPU.
- **O4 (CI):** add a step that fetches the five pinned model files, and run model tests with `SR_REQUIRE_MODELS=1`. DirectML tests need a GPU runner.
- **Doc fix:** 02 §2.4 should say a query model/dims mismatch returns `ModelIncompatible` (not `Internal`).
- **Reviewed-unsafe list:** add `sr_providers::dml` (Win32 calls).
- **Engine wiring:** forward `ProviderEvent::EpChanged` → `ep.changed` (diagnostics), `EmbedBatch` → metrics. Tests without ORT use `sr_providers::testing::HashEmbedder`.
- **Throughput note:** measured under 100% CPU contention. Re-benchmark the CPU on an idle machine in the M1 tester run (DirectML 142–233 chunks/s, CPU ~3/s contended vs ~10/s idle in M0).

## From M1-F (eval harness) — reported
- Gate to beat (file-level nDCG@10, Selene V2 as-shipped, bge-large fp32): selene-src 0.741 (identifier 0.827 / NL 0.748 / path 0.523); CQADupStack-webmasters 0.275.
- Engine brief: sr-core must ship a small bench binary speaking the xtask `cmd:` line-JSON protocol (see m1-f-eval.md §3) so `cargo run -p xtask -- eval --engine cmd:...` can gate M1 (`--min-ndcg-delta 0`).
- MCP brief: `search` tool returns `structuredContent.hits[]` with `path`, `line_start`, `line_end` (needed by the `mcp:` adapter).
- Index path tokens (V2 never does; path queries are its weakest class) — matches the tantivy `path` field in 02-architecture D6.
- Use standard BGE/arctic usage: CLS pooling, query-only instruction prefix, no doc prefix; avoid tiny chunks. V2's setup loses ~12 nDCG points from the same weights.
- Keep dense recall@50 in mind: V2's lexical arm hurt recall@50 (0.395 vs 0.506 dense-only); RRF weights must be tuned against the harness, not assumed.
- Amendment 3.4 (int8) re-check must run on sr-core's own fp32 vs int8 builds (`--max-ndcg-loss 1.0`). V2-pipeline result: −0.44 / +0.04.
- Workspace: add `[alias] xtask = "run -p xtask --"` to .cargo/config.toml (wave-2 owner).
- CI: nightly eval job once an index can be built in CI. Consider Git LFS for the ~14 MB baseline JSON.

## From M1-C (client/bridge/launcher) — reported
- Engine must implement `PipeService` (events, snapshot, serve_mcp, call, drain). Daemon start order: HostLock::acquire → packaged_context_error() → Engine::start (incl. replay + finalize_applied) → pipe::serve(PipeServerConfig::new(dir), svc) — pipe opens LAST (open pipe == ready) → h.stopped().await; idle rule via h.sessions()/idle_for()/begin_drain().
- sr-daemon CLI: `--data-dir <dir> --launched-by <method>`, Windows subsystem.
- Engine EventBus must be built on `pipe::EventHub` so pipe clients get live events.
- Engine must set restricted ACLs on the data directory.
- sr-engine now depends on sr-client (shared pipe-name/permission definition) — accepted.
- Task name is `\SR\daemon-<sid8>` (per-user, avoids cross-user collision) — record as amendment 4.x; update 02-architecture D1.
- sr-launch ships as a 2nd bin in the sr-bridge package.
- Decide: sr-common blake3 features pull rayon-core + memmap2 into the bridge — gate them behind a feature (default-off for bridge).
- Fix two dead-code clippy errors in sr-providers (M1-D) so `cargo clippy -p sr-engine -D warnings` passes.
- Roadmap: close R26; VM-pending items unchanged (MSIX Claude, Cursor, 2nd account, attach race under load).

## From M1-B (sr-extract + sr-extract-client) — delivered
- Engine/claim loop (M1-E/wave 2): wire `ExtractPool` via `SandboxFs` handles (worker never receives a path). Map `Disposition::DeadLetter` -> file dead-letter (`synced_with_errors`), else retry with backoff. Call `pool.shutdown()` on daemon drain.
- `ExtractPool::start` runs `ensure_install_acl` (inheritable ALL APPLICATION PACKAGES RX on install dir, no per-file grants) in every sandbox mode. Root cause: any per-package ACE makes every low-IL token use the AppContainer access check (cf. electron#54382).
- Diagnostics must surface `fallback_reason` (AppContainer -> low-IL) and `pdf_engine_error`; low-IL fallback still allows reads and network.
- Text contract: text kinds verbatim; PDF pages split by `\f`; XLSX `Sheet: <name>` + TSV; PPTX `Slide <n>`. `truncated` = index what came back.
- Deviations to record in 00 (Amendment 4): text/md/code also extracted in worker; html2text dropped for a single-pass tokenizer; supervisor is its own crate `sr-extract-client`; install-ACL check in all modes; `/STACK` link flag added to the shared CRT build.rs.
- M2: NSIS sets the install-dir ACE; uninstall calls `delete_app_container_profile` (3.17); pdfium fetched with a pinned hash.
- Clean VM: re-measure throughput (saturated machine gave ~670–700 small PDFs/s with 2 workers).

## From M1-E (sr-core: chunking, Lance, tantivy, IndexWriter, hybrid search) — reported
- Exit target met on synthetic data: 1M × 384, hybrid p95 41.8 ms, dense recall@10 0.999 (whole collection); scoped 0.997–1.000, dense p95 ≤ 63 ms; single 1% folder uses exact scan, p95 130 ms. Re-check on real embeddings via the eval harness.
- sr-core does **not** depend on sr-catalog/sr-providers. Engine writes adapters for `OutboxSource`, `ChunkSource`, `Embedder`, `Reranker` (contracts: m1-e-core.md §11).
- Engine wiring: per collection open `VectorStore` + `LexicalIndex` → `StoreRegistry`; `IndexWriter::new(outbox, chunks, registry, cfg).replay()` at startup (before `finalize_applied`), `.drain()` on the group-commit timer **on its own blocking-capable task** (tantivy commits block). Searches via `Searcher::search(&SearchRequest)`. Chunker: `Chunker::from_tokenizer_json(bundled bge-small tokenizer)`.
- Engine owns: group-commit timer (≤ 2 s / 1k docs; debounce ≤ 500 ms to keep 3 s save-to-searchable), Lance/tantivy retention schedule, and installing the single rustls provider (3.13).
- `sr_common::tier::AnnParams` (M1-A file) gets the measured defaults: nprobes 16, ef 100, refine 5.
- Known gap: tantivy `path` tokens go stale after a rename until content changes. Engine: on rename, enqueue a cheap lexical-only re-add for that file (path is a ranking field; V2's weakest class is path queries).
- Workspace: `[patch.crates-io]` lancedb → `patches/lancedb-0.40.0`; `patches/` excluded. Bench data in `target/m1-e/bd` (~2.5 GiB, deletable).
- sr-extract-client has 2 clippy findings → wave-2 hygiene owner fixes them so workspace-wide clippy passes without `--no-deps`.

## Orchestrator decisions for wave 2
- **Tier settings home:** user choices (`tier_override`, `use_graphics_card`, `search_model = auto|<id>`) live in `settings.toml` (schema-generated Basic/Advanced UI, one config schema). Detected facts (`detected_tier`, `hw_snapshot`, active `vector_index` per collection) live in catalog `meta`. Never both.
- **blake3 features:** gate rayon/mmap behind an sr-common feature, default-off; only the daemon enables it. Keeps the bridge small.
- **Bench binary for eval:** `sr-bench-engine` bin in sr-engine speaking the xtask `cmd:` line-JSON protocol.

## W2-S (sr-sync) delivered — follow-ups (2026-10-18)
- Gate green: build, 29/29 tests (5 consecutive runs), clippy -D warnings, fmt. Report: docs/research/m1-w2-s-sync.md; API changes: m1-w2-s-progress.md.
- **For W2-E (engine):** implement `EventSink` for `EventBus`; startup order = open catalog -> start epoch -> replay -> `finalize_applied` -> `SyncService::new` -> `startup` -> spawn `run` + `run_scheduler`; pass `DangerousRoots::from_env(Some(data_dir))`; executor must honour `payload.force`; `scan_folder`/`delete_file` jobs are handled inside sr-sync and never reach the executor; `FolderBehavior` gained fields; `triggered` mode now has `watch=true` (Selene semantics); folders must be added via `SyncService::add_folder`.
- **Unsafe review list:** add `sr_sync::fsid` (two Win32 calls for the 128-bit FILE_ID_INFO).
- **sr-catalog backlog (worked around in sr-sync):** latest-scan-id query; claim that skips paused folders / release without consuming an attempt; list files-only files without counting them as skipped; 64-bit volume serial (ReFS).
- **UI / R-UX candidate:** hide `skip_reason=files_only` from the skipped-files list, or a files-only folder looks fully skipped.
- **M2:** notify-based `FolderWatcher` with cached per-folder sandbox; lexical-only re-index on rename (stale tantivy path tokens); mapped network drive detection.
- Perf note: 500-event burst test ~50 s under 100% CPU contention, <3 s idle — re-measure in the M1 tester.

## From W2-H (hygiene/CI), 2026-10-18
- W2-E: enable `sr-common/parallel-hash` in sr-engine (root blake3 entry no longer carries rayon/mmap features).
- W2-E: add sr-daemon delay-load `build.rs` + `harden_dll_search()` first in `main()`; `cargo xtask check-delayload` must pass on the real daemon.
- W2-E: map `AnnParams` (nprobes 16, ef 100, refine 5) -> `AnnConfig`; reconcile partitions (~1000 at 1M via sqrt(N) vs 61 used in M1-E bench).
- W2-S/any crate calling blake3 parallel APIs directly must declare those features itself.
- M2 packaging: build shipped exes in a cargo invocation without sr-daemon (else rayon leaks into bridge); `check-crt` already does this.
- Pending user decision: deny.toml ignores RUSTSEC-2024-0436 (paste) + RUSTSEC-2024-0449 (unsafe-libyaml), both via pinned lancedb, recheck 2027-01-18.
- Pending user action: copy `ci.yml` to `<repo root>/.github/workflows/selene-retrieval-ci.yml` (outside project scope).
- Expected CI reds until W2-E lands: fmt in engine/sync, sr-mcp-server compile, check-delayload.

## W2-E (engine, daemon, MCP server) follow-ups — recorded by orchestrator

Status: W2-E done. Gate for `-p sr-engine -p sr-mcp-server`: build, 35 tests, clippy -D warnings, fmt all pass.
Eval (file-level nDCG@10, real engine, bundled bge-small hybrid): selene-src 0.897 vs V2 0.741 (PASS, +15.6, CI +11.5..+19.7);
CQADupStack-webmasters 0.363 vs 0.275 (PASS). Dense-only 0.793 (lexical arm earns ~10 pts on code). int8 vs fp32 on CPU: +0.24 (PASS on selene-src).

Open items for M1 close-out (owner: close-out agent CO-1, then the M1 tester):
1. sr-sync: on rename, enqueue `index_file {"path_refresh": true}` (executor already supports it). Closes M1-E rename gap.
2. sr-common: typed `tier_override` and `use_graphics_card` settings in `settings.toml` schema; sync `AnnParams` defaults with sr-core measured values (single source of truth).
3. Workspace: move pinned `rustls` into `[workspace.dependencies]`.
4. Mandated startup order gains a step: copy store watermarks into the catalog cache BEFORE `finalize_applied` (W2-E bug fix). Record in 00 Amendment 4 and 02 §startup.
5. Folder status is computed live (no cached status).
6. Release-build latency on an idle machine still to be measured (debug p95 33–81 ms under full CPU load).
7. CQADupStack int8 arm still to run before switching the bundled file to int8 (Amendment 3.4 stays fp32 `model_optimized.onnx` until then).
8. Defender quarantined 5/17,405 CQADupStack docs (quoted malicious JS) — benchmark corpus caveat only.
9. Pending user decisions surfaced by W2-H: cargo-deny exceptions `paste`/`unsafe-libyaml` (accepted provisionally, recheck 2027-01-18); CI file copy to repo-root `.github/workflows/` (NOT done — outside project scope).

## M2-W follow-ups (live sync, reported 2026-10-18)
- **For M2-R (engine wiring):** call `sync.start_watcher(cancel)`; wire the approvals API (app RPC + MCP tool), the pause/resume/cancel RPCs, and `files::skipped_files` for the "skipped" list (files-only files are now counted as listed). The API is in `m2-w-progress.md` under "API ready for engine wiring".
- **M2-R's own code, two failures flagged by M2-W:**
  - `crates/common/src/events.rs:386` `CatchUp::percent` fails clippy `manual_checked_ops`. Fix it so the gate no longer needs `--no-deps`.
  - `crates/engine/tests/engine.rs:373` doesn't compile: it compares `[u8]` with `&[u8]` and needs `*needle`.
- **Risk:** notify 8.2.0 drops buffer overflows on Windows without reporting them. The defences are a burst of 256+ events treated as a rescan, a re-arm every 10 min and a root check every 2 s. Move to notify 9 once it's stable (sr-app shares the pin). Add to the roadmap as a risk.
- **Docs:** add `GetDriveTypeW` to the reviewed-unsafe list in 02 §2.6. R6 can cite m2-w-sync.md.
- **Deviation:** a request while MCP `add_folder` is turned off returns `ApprovalDenied`. None of the 37 codes means "turned off", so the UI wording needs a check.
- **Privacy incident:** M2-W put the user's email in a crates.io User-Agent header once, then switched to a generic one. Every brief must say: never put the user's email in any request.

## M2-A follow-ups (desktop app, reported 2026-10-18)
- **M2-R R0 (blocker for workspace clippy):** `sr-common/src/events.rs:386` `CatchUp::percent` fails clippy `-D warnings`.
- **M2-R R1:** add a daemon `estimate_folder` app-RPC; the app currently counts files itself (stub).
- **M2-R R2-R4:** tier in status snapshot; folder name in error messages; see docs/research/m2-a-app.md.
- **M2-G:** app-RPC to start a model download (enables the "Use it" button), with consent + progress events.
- **M2-I:** uninstaller must delete `HKCU\...\Explorer\StartupApproved\Run` value left by the autostart plugin, plus the WebView2 profile dir.
- **Roadmap:** proposed R-UX20..25 in m2-a-app.md, to be merged into 04-roadmap.md by orchestrator.
- **Privacy rule (all briefs):** never put the user's email in any request header/UA/payload. Two agents (M2-W, M2-A) did this once each in crates.io version checks.

## M2-G follow-ups (GPU + models, done)
- **M2-R:** fix clippy in `crates/common/src/events.rs:386` (`CatchUp::percent`; one-line fix in m2-g report §8). Fix `crates/engine/tests/engine.rs:373` compile error. Reuse startup GPU detection in `detect_tier` (G5). Move re-embed onto jobs queue as `rebuild_profile` (G3, body = `reembed::run`).
- **M2-W/catalog:** add the read-only queries inlined in `reembed.rs`, a `model_files` repo, and fix the error code from `check_query_dims`.
- **sr-core:** IndexWriter should drop references to retired tables (G2); startup sweep is the backstop.
- **M2-A:** wire "Use it" to `use_recommended_search_model` / `choose_search_model` / `cancel_search_model_change` / `search_model_status` (API in m2-g report §6).
- **Workspace:** `reqwest` is now ring-only (aws-lc removed, Amendment 3.13 restored).
- **Docs:** add M2-G's new reviewed-`unsafe` modules (DXGI, version.dll, metered probe) to 02 §2.6 (list in m2-g report §7).
- **M2 test machines:** real TDR, integrated GPU, 4 GB card — not run here (fault injection only).

## M2-I follow-ups (installer, reported 2026-10-18)
- M2-A: merge tauri.conf.json requests from m2-i-progress.md ("Requests for M2-A"): beforeBundleCommand `cargo xtask package --stage-only`, resources to install root, installerHooks, signCommand, checkbox wording. Keep connect/{mod,claude}.rs free of Tauri imports (sr-setup compiles them verbatim).
- Bundle WebView2 bootstrapper/offline installer in the standalone NSIS path (orchestrator decision: bundle, +1.8 MB).
- M2 tester: measure eval + search latency on an `--opt-size` daemon (236.3 -> 150.7 MB). If speed holds, make it default; else restate R25 as ~120 MB download / ~333 MiB installed.
- Add sr-setup's unsafe modules (process scan, task-folder COM, known-folder lookup) to 02-architecture §2.6 reviewed-unsafe list.
- sr-setup is a nested workspace: CI must build/test/clippy it explicitly (or add to root members).
- A test leaks AppContainer profile `SeleneRetrieval.Extract`; find and fix.
- M3: remove keyring/Credential Manager entries on uninstall.
- Release CI: Artifact Signing with SR_REQUIRE_SIGNING=1. Current installer is UNSIGNED.
- Fat LTO OOMs on the daemon (69 min); packaging uses thin LTO + codegen-units=1.

## Owner decisions 2026-10-18 (Amendment 5): work for the M2 tester / M2 wave 2
- **CI is live at the repo root** (`.github/workflows/selene-retrieval-ci.yml`). Any edit to the project's `ci.yml` must be mirrored there (identical below the header). Agents still never commit.
- **Add an `installer-smoke` job** to both copies: `windows-latest`, needs `check`. Build the package (`cargo xtask package`, unsigned), install silently per-user, run `installer/verify` (install → task → MCP round trip → uninstall keep/delete → nothing left), plus a zero-config first run with a network-endpoint watch. Upload the setup exe and verify log as artifacts. Note in the job comment that hosted runners already carry the VC++ runtime, so `check-crt` remains the "no runtime" proof.
- **Signing deferred (5.2):** keep the hooks; packaging must succeed and label output UNSIGNED with no credentials. No task may block on signing.
- **User-test kit (5.4)** (internal user-test notes, not published): task card (one page, plain language), success criteria, observation form, SmartScreen "More info → Run anyway" screenshot note, and a results template the owner fills in. No telemetry, no personal data collected.
- **Privacy rule (repeat):** never put the owner's email or any personal data in any request header, URL or payload.

## M2-R follow-ups (engine robustness, done 2026-10-18)
- **Done:** 4.24 ready-before-replay (133 ms engine start steps after model load), 4.25 bounded group commit (2 s / 512 chunks / 1000 ops; drain <= 10 s), R-UX18 no-match cut-off (dense < 0.65 AND BM25 < 20; bge-small only; nDCG unchanged 0.897 / 0.363), R-UX19 notices, error text "Title. Body Next-step", R9 WAL truncate after purge, watcher wired. Clippy R0 (`CatchUp::percent`) fixed.
- **O1 (M2 tester / wave 2):** daemon `initialize` measured 2.4-2.8 s from spawn under load; ~1.7 s is GPU detection + model load before the pipe opens. Open the pipe before model load (serve status/handshake; search waits or returns Starting).
- **O2 (M2-G code):** a startup task (`on_engine_ready`) holds an untracked engine handle; caused one flaky failure in `zero_config_start_runs_steps_in_order` (tantivy lock on restart). Track/join it at drain.
- **O3:** a very large file interrupted at shutdown is re-embedded from scratch (acceptable for M2; note as risk).
- **O4/O6:** cut-off floors calibrated on two English corpora for bge-small only; tiny folders may have low BM25. Cut-off disabled for other models until calibrated.
- **O5:** no "dismiss" for notices yet (M2-A UI + engine RPC).
- **Open from M2-A still unaddressed by M2-R:** R1 `estimate_folder` RPC, R2 tier in status snapshot, R3-R4 folder name in errors - verify in tester, else wave 2.
- **M2-A must consume:** status `catch_up {done,total}` and `notices`; events `catch_up_progress`, `notice`; search `no_good_match`.

## M3-U follow-ups (2026-10-19)
- M3-Q: R-Q1–Q6 from m3-u-progress.md (watch_note + removing flag in folder status; full-screen wait signal; consent-needed events; provider/consent/answers RPCs; launch `sr-app --approve` when no app attached).
- M3-P: confirm Credential Manager prefix `SeleneRetrieval/` (R-P2); sr-setup creds.rs depends on it.
- R-X1 (unowned xtask): fetch offline WebView2 runtime + pass SR_WEBVIEW2_OFFLINE. ORCHESTRATOR DECISION PENDING: offline runtime adds ~170-180 MB (R-UX34) vs R25 size budget.
- Tester: regenerate ui/src/bindings.ts via `SR_UPDATE_BINDINGS=1 cargo test -p sr-app bindings` (hand-written in M3-U); compile installer/sr-setup + NSIS in the gate; live-walkthrough steps in m3-u-app.md §6.

## M3-P follow-ups (providers, del-1792357415688-4) — for M3-Q and the M3 tester
- M3-P tests were compile-checked only; the M3 gate is their first real run (remote_contract.rs + 9 test targets).
- M3-Q / engine: enable `sr-providers/remote` in sr-engine; back the consent gate with the catalog via `MemoryGate::with_sink`; grant key-check consent before `check_key`; emit `consent.needed` when a call is blocked.
- Root Cargo.toml: `genai` now `default-features = false` (its defaults pull banned aws-lc). Accepted by orchestrator.
- genai bug fixed: model names ending -max/-low/-high were truncated; regression test present.
- Docs 02 §2.6: add `sr_providers::remote::proxy::win` to the reviewed-unsafe list.
- CI (both workflow copies): add `cargo xtask sync-providers --check`.
- Roadmap: log R-UX34 (NTLM/Kerberos proxies unsupported; Basic only) and M3-P open items.
- MiniMax / BlackBox key check = one-token chat (max 1 output token cost).
- Fixtures are documented-format synthetic, not live captures; refresh against real accounts before GA.
- Credential Manager prefix `SeleneRetrieval/` matches M3-U's uninstall cleanup.

## M3-Q follow-ups (for M3 tester / gate)
- sr-app `crates/app/src/state.rs`: add `..EngineStatus::default()` and `..FolderStatusSummary::default()` (new shared status fields) or sr-app won't compile.
- P-2 (M3-P loader): model loader picks tokenizer files by tier; multilingual-e5-small pins fail. Loader must take tokenizer pins from the model profile. Until fixed, choosing multilingual -> "Couldn't switch search model" (search unaffected).
- Run all new tests (none executed yet: M3-P, M3-Q, M3-U all check-only).
- Evals: selene-src nDCG@10 must stay >= 0.897 with exact lane on; run R-UX33 23 positive / 16 negative sets (`--exact-lane on|off`); enhancement parity fixtures (eval/enhance).
- Live checks: m3-q report §7.
- Proposed risks: (a) any same-user process can grant consent/approve folders via app pipe; (b) provider connect before Ready can slow startup under slow proxy auto-detect (WPAD).

## M3 tester follow-ups (for M4; details in m3-test-report.md)
- Next dist build picks up the three post-dist fixes (sr-core single-exact-term no-match + whole-term-first ordering;
  MCP add_folder reply text). Rerun `installer/verify/roundtrip.ps1` on it.
- R-UX36 reranker: tune (top-N ≤ 20, skip path/identifier queries, GPU) before surfacing the Advanced switch.
- R-UX37 exact lane: weight by query shape; re-run selene-src + CQADupStack.
- R-UX38: per-provider "Get a key" link in the manifest; OAuth design pass (Codex, Claude Code, Antigravity).
- R-UX39 UI: reranker switch, multilingual offer (page 12, real size 236/471 MB), max rounds (page 13); the app's
  `tier_of_model` should handle the multilingual index; show `client_name` on approval cards ("An app").
- R31 app-pipe peer check; R32 provider connect after Ready / bounded WPAD.
- xtask package: update the printed R25 band (still M0's 106–114 MB / 285 MiB).
- sr-providers dml test leaks `%TEMP%\sr-m1d-renamed-*` dirs (one per run).
- Pending checks: corporate-proxy VM, real-account fixture refresh, Claude Desktop refresh on list_changed, owner
  walkthrough of pages 20–23 + OQ-16, R-UX13 consent-wording user test, e5 fp16 DML-vs-CPU cosine.

## M4-A follow-ups (2026-10-19)
- For M4-E: R-E1 add `key_url` per provider to the provider list (page 21 link stays hidden until then). R-E2 add `files_only`/`indexing_mode` to folder rows.
- For M4 tester:
  1. Regenerate bindings: `SR_UPDATE_BINDINGS=1 cargo test -p sr-app bindings`, then typecheck and lint again. The current bindings were written by hand.
  2. `cargo test -p sr-app`, `cargo test --manifest-path installer/sr-setup/Cargo.toml`, clippy on both.
  3. Live walkthrough per m4-a-app.md section 9. Debug runs need `SR_TRUST_ANY_APP=1` for trusted-app RPCs.
- Deviations accepted: answers arrive in one piece in M4 (phase lines, no streaming); search results offer "Show in File Explorer", not "Open" (never runs .bat/.ps1); Reconnect lives on the in-app card, not the notification; diagnostics are saved as .txt; folder groups have no per-group model.
- Risks to merge into the roadmap: Cursor installed for all users and portable VS Code aren't detected; sticky search bar may overlap at 200% text size.

## M4-E follow-ups (for M4 tester), 2026-10-19
- First real run of ~3,000 new engine lines: expect a clippy fix batch. Nothing was tested; only `cargo check -p <crate> --tests` + rustfmt.
- R31: privileged pipe methods (consent.set, approvals.approve, providers.save, scope changes, HTTP on/off/tokens, index move) answer only our installed sr-app.exe / sr-daemon.exe. Release-build scripts calling them (e.g. M3 `liveprov.mjs`) will get PermissionDenied: use a debug daemon with `SR_TRUST_ANY_APP=1`, the admin CLI, or the app.
- Docs: add `process_image_path` to 02 §2.6 reviewed-unsafe list; add catalog V4 tables (folder groups, activity log, client token hint) to 02 §2.4.
- M4-A hand-wrote TS bindings: regenerate with tauri-specta and reconcile with the contract's "Changes" section (`files_only`, `tier.multilingual`; `approval_expired` defined but never sent; HTTP tools show "Last used" not session count).
- Page 31 "Change search model" must open Settings → Search (no per-group model in M4; groups are labels over the default collection).
- Open, not done by M4-E: per-provider `key_url` in xtask provider table (M4-A request R-E1); exact-match lane query-shape weighting (R-UX37, needs both eval corpora); `xtask package` R25 band text.
- New deps declared in crate manifests (hyper, hyper-util, http, http-body-util, bytes, tower-service in sr-mcp-server; getrandom in sr-engine); Cargo.lock gained only `httpdate`. Run cargo-deny.
- Live checks: HTTP via curl (401/403/404, initialize, revoke, switch-off), index move to a second drive and back, per-client scopes, Activity push, Ask/Enhance cancel on app close.
- Risk notes for roadmap: moved-index drive missing → service refuses to start; local-app scopes are privacy, not a security boundary (HTTP is per-token enforced); R31 residual (same-user injection) until signing (M5).

## M4 tester follow-ups (for M5; details in m4-test-report.md)
- R34 wording: a missing moved index prints the added-folder sentence ("Ask the user to add the folder"); give it its
  own text + "Move it back" (needs the old copy or a re-index).
- R-UX43: name an unreadable (JSONC) VS Code mcp.json in the uninstall summary; NSIS detail line says "completes on
  first start" for uninstall notes.
- P2 low items: Deeper-answers link should land on the Answers card; in-app [Ask] hidden by a switch worded only for
  other apps; size-limit help mixes KiB/MiB; "token" vs "password" on page 29; re-read confirm drawn twice with
  Advanced open; failed restart toast has no action; Move-index failure panel lacks [Retry]; no UI for
  `clients.forget`.
- Gates: after a clippy failure, run clippy-only passes until clean before the rerun (a failing crate hides its
  dependents even with `--keep-going`).
- M4-E D7: move hyper/http/bytes/tower-service/getrandom to root `[workspace.dependencies]`.
- Pending: 3-new-user test, owner WebView2 walkthrough of pages 24–32, VM matrix, signing.


## M5-S security review follow-ups (orchestrator, 2026-10-19)
Source: docs/research/m5-s-security-review.md, m5-s-provider-signin.md. Fix in the M5 tester's first batch (code only, no extra builds; covered by the single end-of-phase gate):
- S-01 (High) sr-client attach: verify pipe server owner/process SID == current user before sending anything (GetNamedPipeServerProcessId + token check). Regression test with a pipe created under a different owner/label where possible.
- S-02 (Med) sr-engine index move: refuse a pre-existing target folder not owned by the current user; reset owner + use /inheritance:r with a clean grant (no surviving explicit ACEs).
- S-03 (Med) sr-mcp-server HTTP: if the remembered port is taken, do NOT silently move; show "Port in use" notice and leave HTTP off (tokens stay bound to the configured port).
- S-04 (Med) sr-extract-client: low-IL fallback only for "AppContainer unsupported" errors, never for transient spawn failures/timeouts; retry AppContainer; surface fallback in diagnostics.
- S-05 (Low) add folders.add, groups.move_folder, settings.update_advanced, model downloads, host.shutdown to the R31 privileged set.
- S-06..S-11 (Low): strip x-api-key/x-goog-api-key on cross-host redirects; Basic proxy creds only to explicitly configured proxies, no auto NTLM on PAC fetch; harden_dll_search() first in sr-app/sr-bridge/sr-launch/sr-setup main; answer loop reads limited to retrieved/cited files + fix tag escaping; extend secret denylist (.git-credentials, _netrc, .kube, *.ppk, ...); per-caller resource limits.
- S-12: CI runs cargo deny for installer/sr-setup too.
- Provider list: Claude Code + Antigravity -> not_allowed_by_vendor (API key only); Codex -> pending_vendor_approval (no build until OpenAI written confirmation).
- Roadmap: add R38 (S-01), R39 (S-02), R40 (S-04); update R35 (S-03), R31 (S-05). External review remains a GA prerequisite.

## Owner override, 2026-10-19 (Amendment 10.1), supersedes the Claude Code part of the M5-S provider line above
- Claude Code: do NOT set not_allowed_by_vendor. Keep it listed as oauth_pending_design (Agent SDK OAuth sign-in planned). Any sign-in code stays behind a default-off flag until a record of Anthropic's approval exists (release gate R41).
- UI label: "Claude (sign in)". Never "Claude Code" branding or look-alike visuals.
- Antigravity: not_allowed_by_vendor (unchanged). Codex: pending_vendor_approval (unchanged).
- M5 tester: add R41 to docs/04-roadmap.md along with R38-R40. R41 = Claude Code subscription sign-in needs a record of Anthropic's prior approval before it ships enabled (Amendment 10.1).

## Owner decision #2, 2026-10-19 (Amendment 10.1 revised), SUPERSEDES the section above
- Claude (sign in) is ENABLED BY DEFAULT in every local/private build. Remove any default-off flag/feature gate for these builds. Provider-list state: `enabled` (sign-in via Agent SDK OAuth). Settings entry stays, label "Claude (sign in)".
- Reuse Selene's existing flow, do NOT design a second one: UI states/copy from Selene `app/settings/settings-panel.tsx` + `app/settings/page.tsx` (login, browser opened, paste mode submit/cancel, auth success, logout, account email); protocol and token refresh from `lib/auth/claudecode-auth.ts` + `app/api/auth/claudecode/{authorize,exchange,refresh}/route.ts`. Selene files are read-only references. Rust (sr-providers) mirrors the protocol; tokens in Credential Manager under `SeleneRetrieval/` (uninstall purge already covers the prefix). Never log or print tokens.
- Surfaces: wizard step "Get written answers (optional)" (design-docs/pages/20) and Providers & Keys (page 21). Consent dialog (page 22) still applies before first send.
- Depends on S-01 (pipe owner check) being fixed first, since tokens travel over the app pipe.
- Antigravity: API key only (unchanged). Codex: pending_vendor_approval (unchanged).
- Owners: M5-E/sr-providers + xtask provider table (state + auth kind); M5-A (pages 20/21 sign-in UI); M5-I (nothing new); tester: verify sign-in path with a fake OAuth server, never real tokens.
- M5 tester: write R41 in docs/04-roadmap.md exactly as: "R41 | Claude (sign in) uses Agent SDK OAuth. Anthropic approved it for the Selene app and it is on by default in local and private builds, which are not blocked. Before any public or distributed build, recheck it against Anthropic's Agent SDK terms then in force and keep a record of approval with the release notes (Amendment 10.1)."

## Orchestrator note, 2026-10-19 12:25 (M5 resume #4, Claude Code takes over orchestration)
- Claude (sign in) RPC names are fixed to M5-A's names: `claude_signin.start`, `claude_signin.submit_code`, `claude_signin.status`, `claude_signin.logout` (privileged, R31). M5-E publishes the request/response shapes in m5-e-progress.md under "Claude sign-in contract"; M5-A binds to them.
- `harden_dll_search()`: M5-E exports it from a shared crate (path in m5-e-progress.md); M5-I calls it first thing in sr-app / sr-bridge / sr-launch / sr-setup main. In sr-app, M5-I touches only that line.
- Agents stopped at ~12:17 mid-edit; each resume starts with a crash audit of files changed after its last checkpoint.

## Owner decisions on M5 open items (user, 2026-10-19)
- (a) CI: do NOT apply M5-I's diff to the repo-root .github/workflows. Only the project copy keeps the sr-setup/msi-ca cargo-deny step.
- (b) harden_dll_search keeps the app's install folder in the search list (bundled ONNX Runtime/DirectML); current dir and PATH stay excluded. Accepted as is.
- (c) sr-setup.exe may request a drained host.shutdown without the trusted-peer check. Accepted as is.
- (d) WiX stays on v5.0.2 (no v6/v7 OSMF EULA).

## M5-C, 2026-10-19: CLI sign-in (Amendments 10.2 / 10.3), SUPERSEDES the implementation in "Owner decision #2" and the 12:25 RPC list
- "Claude (sign in)" reuses the local Claude Code login through the user's own `claude` CLI (as Selene's `sdk` backend does); "Codex (sign in)" reuses the local Codex CLI login through `codex exec` (owner decision, overrides `pending_vendor_approval`). No OAuth, no pasted code, no tokens anywhere in Selene Retrieval. Both on by default in local/private builds; release gates R41 (reworded) and R42 (new).
- Provider table: `claudecode` and `codex` rows are `enabled`, `auth_kind: oauth`, **`protocol: cli`**, `validation: cli_status`, no `base_url`, `key_url` = the tool's install page (used by the "isn't installed" link through the `open_link` allow list). Labels "Claude (sign in)" / "Codex (sign in)".
- RPC (all privileged, R31): `cli_signin.status {provider, refresh?}` → `CliSigninStatus {provider, label, authenticated, email, plan, cli_found, cli_version, state: not_installed|signed_out|signing_in|signed_in, tool, install_url, error?}`; `cli_signin.start {provider}` → status + `started`. `claude_signin.status` / `claude_signin.start` stay as aliases for `claudecode`. `claude_signin.submit_code` and `claude_signin.logout` are **removed** (removing the provider only disconnects Selene Retrieval; never signs the user out of the CLI). Event `claude_signin.changed` → **`cli_signin.changed`**.
- `providers.save {provider: "claudecode"|"codex", key: null, address: null, check}` works with no key; `check` asks the CLI's status (nothing is sent); not installed → `ProviderNotConfigured`, not signed in → `ProviderAuth`, both rendered as "{tool} isn't installed on this computer" / "{tool} isn't signed in" (+ Open settings).
- Answer / enhance for these providers: `sr_providers::cli::CliLlm` (one CLI run per turn, prompt on stdin, consent + audit through `Egress`, Job Object tree kill on cancel/timeout, any tool-call event stops the run). Exact flags and reasoning: `docs/research/m5-c-cli-providers.md`.
- Opener: the "Sign in opened Documents" bug was Explorer getting the 379-char OAuth URL; `opener::explorer_arg_ok` now refuses empty/relative/over-long/`,` arguments before Explorer starts.
- Tester: M5-C's fake-CLI tests (`crates/providers/tests/cli_fake.rs`, `crates/engine/tests/cli_signin.rs`, unit tests, `ui/tests/claude-signin.test.mjs`) were only `cargo check`ed (UI tests run: 7/7). Bindings (`ui/src/bindings.ts`) were edited by hand to match specta; if the bindings test differs, regenerate with `SR_UPDATE_BINDINGS=1`.

## M5 tester follow-ups (for M6; details in m5-test-report.md)
- Done after M5-C: full gate g3 green first run, installers rebuilt once, fake-CLI end to end 13/13 (Amendment 11.6).
- Owner real-prompt checks for the CLI providers: Claude 2.1.303 accepts `--setting-sources ""` in `-p` mode; Codex
  ChatGPT login works with `--ignore-user-config`.
- VM matrix (Win10 22H2, Win11 23H2 / 24H2, LTSC without WebView2): NSIS + MSI round trips with `installer/verify/roundtrip.ps1`
  (and `-Msi`), offline variants with no network, uninstall leaves nothing, R-UX43 fake-roots notice, built app's Folders /
  Overview with skipped files (M4 owner bug), S-08 window opens. Not runnable on a PC with the owner's install.
- Second-account tests for R38 (pipe squatting) and R39 (pre-created move target).
- R40: decide on a deny-only SID / WFP filter for the low-IL extractor; consider not treating a *missing* `pdfium.dll` as
  "AppContainer unsupported".
- S-12 housekeeping still open: diagnostics path hashing, clipboard history, config backups, downloader side-file size,
  sr-setup pid reuse, case folding.
- R34 live script: `index.move.done` can be emitted before a re-attached app subscribes; the app should read
  `index_location.status` on attach (it does) — tests should too.
- Live scripts: never pick "the only sr- pipe" (Amendment 11.11); `engine.status` returns a Snapshot `{seq, stream, value}`.
- Pending owner: real-account Claude / Codex CLI sign-in, Narrator checklist, 3-new-user test, external security review,
  signing + updater (R-UX24).

## Owner confirmation, 2026-10-19
- R41 (reworded by M5-C for the CLI design) and R42 (Codex) in docs/04-roadmap.md are approved as written. They replace the OAuth-era R41 sentence from Owner decision #2.

## Model refresh for the CLI sign-in providers, 2026-10-19 (owner request; Amendment 11.16)
- `cargo xtask sync-providers` now also reads Selene's `lib/auth/claudecode-models.ts` and `lib/auth/codex-models.ts`
  (read-only) and writes a `models: [{id, name}]` list (Selene's order and labels) into the `claudecode` and `codex`
  rows of `crates/providers/data/selene-providers.json`. Claude: all 11 ids. Codex: 85 ids; left out are the 8 ids Selene
  labels "(Legacy)" and `gpt-5.1-chat-latest` (an alias `MODEL_MAP` sends to `gpt-5.1`). Other rows are unchanged.
- Defaults are an SR policy override in the xtask `POLICY` table (`defaults`), not Selene's stale `DEFAULT_MODELS`:
  Claude (sign in) `claude-sonnet-5-5` / utility `claude-haiku-4-5-20251001`; Codex (sign in) `gpt-5.5` / utility
  `gpt-5.4-mini-low` (GPT-6 Astra and GPT-5.6 are offered but not the default until a Codex CLI that accepts them is
  verified). The xtask fails if a default isn't in the list. A model the user saved earlier is kept as saved.
- `ProviderSpec.models` + `model_hints()` feed `list_models` / [Check and save]; `providers.status` for a CLI row reads
  the list and the default from the table, so an existing install sees the new list without [Check again].
- Codex: `split_model` knows `none` / `minimal` / `low` / `medium` / `high` / `xhigh` / `max` / `ultra`; a listed id with
  its own effort variants (`gpt-5.1-codex-max`) is never split. `gpt-6-astra-max` runs as `-m gpt-6-astra -c
  model_reasoning_effort=max`.
- UI: the Providers page picker shows Selene's labels ("Automatic (Claude Sonnet 5.5)"); a saved model no longer in the
  list stays shown; "Answers will use …" lines use the label. No free-text model field (there was none before).
