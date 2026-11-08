# 08. Broad "how does X work" questions under the strict passage budget

Status: Plan, approved by owner 2026-10-20 (orchestrated; proceed). §9: follow-up on the owner's own wording, same day.

> Binding: Product Principles P1 (average-user simplicity, never weakening security) and P2 (standing check), see `product-principles.md`. Amendments in `00-review-and-decisions.md` stay binding. Decisions: Amendment 14.

Scope: the owner's feedback of 2026-10-20 on the docs/06 build. (1) The broad question "How does vector indexing work?" got a worse answer; (2) `grep` still used the old output shape; (3) `spikes/gpu-tier/results/detect.txt` still appeared; (4) the index showed "501 of 524 files, synced_with_errors, 11 skipped". Out of scope: the `maxResults` defaults and the meaning of `maxSources` (docs/06 issue 1 stays: it is the hard total of passages the model receives).

The exact question was not found anywhere in the repo, eval or docs, so the owner's wording is used: **"How does vector indexing work?"**

## 1. Method

- **Reproduced on my own daemon, never the owner's.** The `dist` exes from the last milestone (`target/package/stage`, the build the owner installed) were copied to `%TEMP%\sr-rank\app-before` and run with `--data-dir %TEMP%\sr-rank\data` (own pipe, HTTP off). The folder is a copy of this repo with its `.seleneignore`, without `target/`, `installers/`, `vendor-bin/`, `crates/providers/models`, `ui/node_modules` and the vendored MSVC (`spikes/extract-sandbox/vendor`). The owner's install, pipe and port were not contacted.
- **`answer` through a local stub.** Provider `ollama` with `network.base_url` pointed at an OpenAI-compatible stub on 127.0.0.1 (consent rows written into the test catalog only). The stub logs every numbered `<source>` it is shown and echoes them in its answer; it can ask for one `read_file` and send `search_again` queries. No real model and no claude/codex CLI was used.
- **Same index for before and after.** The index reproduced the owner's numbers exactly: **501 of 524 files, `synced_with_errors`, 11 "skipped"** (§5).

## 2. Findings

### 2.1 Where lance.rs ranks (first stage)

`search` for the exact question, fused order (hybrid, release, bundled bge-small):

| Rank | Passage | Note |
|---|---|---|
| 1 | `crates/providers/tests/local_cpu.rs:60-90` | test |
| 2 | `spikes/gpu-tier/stlshim/vector_algorithms.cpp:950-972` | vendored Microsoft STL source |
| 3 | `eval/v2-baseline/v2_search.py:1-20` | eval script |
| 4 | `crates/providers/src/local.rs:52-76` | embedding provider |
| 5 | `crates/catalog/src/schema.rs:67-97` | |
| 6 | `eval/v2-baseline/verify_port.mjs:40-58` | eval script |
| 7 | `crates/providers/src/embed.rs:139-167` | embedding provider |
| 8, 9 | `vector_algorithms.cpp` (×2) | vendored |
| 10 | `crates/providers/tests/tier_models.rs:39-67` | test |
| 11 | `vector_algorithms.cpp` | vendored |
| 12 | `crates/core/src/index/writer.rs:206-233` | **first core index file** |
| 13, 14 | `vector_algorithms.cpp` (×2) | vendored |
| 15 | `vector_algorithms.cpp` | vendored |
| 17 | `crates/core/src/index/lexical.rs:1140-1162` | core (a test module inside it) |
| **19** | `crates/core/src/index/lance.rs:699-723` | **first lance.rs passage** |
| 23, 32, 36 | `lance.rs` | 17 lance.rs passages among the top 150 |
| 30 | `README.md:1-7` | |

(The list with `maxResults: 20`; with 60 the first lance.rs passage is 20th because the lanes go deeper.) `docs/02-architecture.md` is not in the top 150: its passages about the index are a few lines inside long sections. `crates/core/src/index/mod.rs` first appears at 44, `query/mod.rs` at 91.

- **One file takes many slots.** `vector_algorithms.cpp` (12,700 lines of Microsoft STL, copied for a link shim) holds 6 of the top 15 and 17 of the top 150. Test files hold 2 of the top 10.
- **lance.rs is the file with the most evidence, not the best single passage.** Grouped by file (top 150), lance.rs has 17 matching passages (as many as the STL copy); summed over its best three it is the 3rd file (2nd without the STL copy), yet its best single passage is 19th.

### 2.2 Budget split and what the model received

`first_search_sources` (`crates/core/src/answer.rs:164`) gives the first search `maxSources − ⌊maxSources/4⌋`; reads and deeper rounds share the rest. Captured stub prompts:

| `maxSources` | First search | Reads / deeper rounds | lance.rs among the passages sent |
|---|---|---|---|
| 8 | 6 | ≤ 2 | no |
| 12 (default) | 9 | ≤ 3 | no |
| 20 | 15 | ≤ 5 | no |
| 20, before docs/06 issue 1 | 20 (the whole `maxSources`) | + up to 6 reads, + deeper rounds | **yes, #19** |

Default answer, first stage (9): `local_cpu.rs` (test), STL ×3, `v2_search.py`, `local.rs`, `schema.rs`, `verify_port.mjs`, `embed.rs`. That matches what the owner saw: the answer could only cite a README-level or provider-level description and said it didn't know whether an ANN index is used.

### 2.3 The owner's hypothesis

**Confirmed in substance, with one correction.** Before docs/06 issue 1 the first search took all 20 passages and lance.rs was #19 of them; now it takes 15 and lance.rs is cut. The stricter budget removed a file that was only just inside.

The correction: the read round can't have rescued it. A `read_file` step may only read a file that is already among the sources (S-09, `source_file_allowed`, `crates/core/src/answer.rs:602` and `:679`), so a read adds depth to a file the first search found, never a new file. The stub's read round re-read source 1 (`local_cpu.rs`) and could not reach lance.rs. A deeper round (off by default) did reach it with targeted queries ("vector index implementation" brought in 4 lance.rs passages, sources 17–20 of 20). The "#23" the owner remembers fits a read excerpt of lance.rs after the first search had already listed it near #19.

### 2.4 Root cause

`EngineHandle::answer` searched exactly `first` passages and used them in fused order (`crates/engine/src/answers.rs`, before this change; the cut is `hydrated.truncate(max_results)`, `crates/core/src/query/mod.rs:778`). Fused order is passage order: nothing stopped one file from taking most of a 9-passage budget, nothing preferred implementation over tests for a conceptual question, and a file whose many passages each match a little lost to files with one strong passage.

## 3. Candidate fixes

| Candidate | Effect on the exact question (simulated on the captured top 150) | Trade-offs | Verdict |
|---|---|---|---|
| A. Per-file cap in the first stage (1–2 passages per file) | lance.rs at 12–14 of 15; still out of 9 | Simple; frees the STL copy's slots, but the next files in line are eval scripts and providers | Part of the fix (as round robin) |
| B. File-role prior: tests / fixtures / benches / examples × ½ (unless the question is about them); README and docs neutral | Alone: lance.rs 11 of 15 with A; out of 9 | General and cheap; risks a "how is X tested" question (handled: the prior is off when the question names tests, specs, fixtures, benches, examples or mocks) | Part of the fix |
| C. Retrieve wider than the budget, rerank, send at most the budget | Needed for A, B and D to have something to choose from | The search costs a few more hydrated ids (30 instead of 9) | Part of the fix |
| D. File-level evidence: score a file by its best passages (best + ½ second + ¼ third) | **lance.rs 3rd** of 6, 9 and 15 (2nd file without the STL copy); `writer.rs`, `index/mod.rs`, `lexical.rs` also in 15 | Long files with many weak matches gain (RRF scores are flat: ½ × a second passage outweighs 40 ranks). Measured: CQADupStack −1.9 / −3.9 nDCG@10 at k 10 / 50 | Part of the fix, with small weights (0.1 / 0.05, §8.1) |
| E. Revisit the ¾ first-stage split | More first-stage room doesn't help when the order is wrong (lance.rs needs 19) | Reads are the only way to add depth to the files found; deeper rounds are off by default | Not changed |
| F. Query-type detection | Lookups (one identifier, path, file name or number, `QueryShape::is_lookup`) must keep the fused, exact-term-first order | The existing shape test is robust (R-UX33 / R-UX36) | Used only as the off switch |
| G. Symbol awareness (boost files whose definitions match query words) | Not needed for this question; needs per-file symbol data at query time | Larger change, index work | Not done |
| H. Change the `search` order itself | Would move the eval baselines and every agent's `search` results | Out of the owner's ask (first stage of answers) | Not done; follow-up |

Repo-only: `spikes/*/stlshim/` in this repo's `.seleneignore` (third-party code, like decision 8's lancedb copy). This is configuration, not product code; the product fix is measured with the STL copy still indexed.

## 4. Chosen fix

**File-aware passage selection for answers** (C + D + a soft A + B, F as the off switch), `crates/core/src/query/passages.rs`:

```text
answer (first search) and each deeper-round search:
  search passage_width(budget) = 3 × budget (at least 30, at most 150) candidates
  if the question is a lookup → the first `budget` in fused order (unchanged)
  else per file (folder + path):
         bonus  = 0.1 × second-best + 0.05 × third-best fused score
         weight = ½ for tests / fixtures / benches / examples unless the question asks about them
       pick greedily by   fused score × weight + bonus × weight − 0.003 × passages already taken
                          from that file
       until `budget` passages are picked
```

- **Iterated on the eval before shipping (§8.1).** The first design (bonus ½ / ¼, strict one-passage-per-file round robin) put lance.rs 3rd but cost CQADupStack 1.9 nDCG@10 points (CI −2.6 … −1.2) and selene-src 4.2 points at chunk level (its golden spans often sit several to a file). The shipped weights keep the file evidence small and the per-file limit soft: a file's second passage still comes in when it clearly beats the other files' best ones.
- `maxSources` keeps its meaning and its hard total (docs/06 issue 1): the first search still sends at most `maxSources − ⌊maxSources/4⌋`, deeper rounds at most 8 per query within the total.
- `search` (tool, app, eval) is unchanged: the selection only decides which passages of a wider search an answer receives. `enhance_prompt` is unchanged.
- No repo paths, no new settings, nothing to configure (P1). Security unchanged: same scope, same S-09 read rule.

`grep` (owner item 1) gets the docs/06 shape (§6). `.seleneignore` (item 2) and the skipped files (item 3) are in §5–§6.

## 5. Skipped and errored files (owner item 3)

Own index of the repo copy: 501 indexed, 12 skipped, 11 errored (`dead`). "11 skipped" in the status is `files_skipped`, which counts files that **couldn't be read** (`crates/engine/src/handle.rs:223`); the MCP text says "11 couldn't be read". The 12 intentional skips are not in that count.

| File(s) | State, code | What the user sees (app folder page / MCP) | Legitimate? |
|---|---|---|---|
| 10 JSON vector dumps (`spikes/native-stack/test11/*.json`, `crates/providers/tests/fixtures/bge_small_ref_st.json`) | skipped, `data_file` | "Looks like a data file (very long lines), not a document" (no Retry) | Yes (docs/06 change 5) |
| `spikes/extract-sandbox/fixtures/long-line.txt` (48 MB), `oversize.txt` (80 MB) | skipped, `too_large` | "Bigger than the 10 MB limit" (no Retry) | Yes |
| `fixtures/binary.txt`, `binary-as.{docx,html,pdf}`, `truncated.pdf`, `zero.pdf` (0 bytes), `nested-xml.docx`, `zipbomb.docx`, `pdf-flate-bomb.pdf` (9) | dead, `FileUnreadable` | "May be damaged, password-protected or in an unsupported format" + Retry; MCP "couldn't be read" | Yes: deliberately broken parser fixtures (M0 sandbox spike) |
| `fixtures/zalgo.txt` (2 MB), `fixtures/bidi-zw.txt` (3.8 MB) | dead, **`InvalidInput`** | "Couldn't be read" + a Retry that always fails again (the error's own text is "That request wasn't valid") | **No: product bug** |

**The bug.** Both are valid UTF-8 text. The tokenizer reads them as almost no tokens (BERT normalisation drops the combining marks and the zero-width / bidi controls, and one 800 KB "word" is a single `[UNK]`), so the 400-token splitter returns the whole file as one chunk, and the catalog refuses chunk text over 1 MiB (`MAX_CHUNK_TEXT_BYTES`, `crates/catalog/src/repo/chunks.rs:24`, `:63`) with `InvalidInput`. The same happens to any text over 1 MiB that the tokenizer reads as a handful of tokens (long runs of combining marks, or words glued together by zero-width or bidi control characters, as some copy-pasted or obfuscated text has). **Fix:** the chunker cuts any piece over `CHUNK_MAX_BYTES` (= the catalog limit, 1 MiB) into windows at character boundaries. Every chunk that could be stored before is unchanged, so `CHUNKER_VERSION` and all chunk ids stay. The two files then index; the status shows 9 files that couldn't be read, all legitimately.

Not changed: the 9 broken fixtures stay visible as "couldn't be read" (honest; they are in the repo on purpose). If the owner prefers "Up to date" for this repo, `spikes/extract-sandbox/fixtures/` can go into the repo `.seleneignore`; not done without asking.

## 6. `grep` shape and `.seleneignore` (owner items 1–2)

- **`grep`** gets the docs/06 shape: `folders: [{root, name, id}]` once, `matches: [{path (folder-relative), line, text}]`, `f` only with more than one folder, `truncated` only when true; `files_searched`, `abs_path` and `folder_id` only with the new `debug: true`. Text: `N matches (in C:\root):` then `rel:line: text` (`k:rel:line:` with several folders). The engine DTO (app-RPC `files.grep`) is unchanged.
- **`.seleneignore`** gains `spikes/*/results/` (with a comment) and `spikes/*/stlshim/`.
- **Generic built-in rule for result dumps: not added.** The measured dumps are `results/*.txt` holding pretty-printed JSON (short lines, so the docs/06 long-line rule can't see them), `*.jsonl` (already ignored) and raw `.bin` vectors (binary, never indexed). The only generic signals are the directory name `results/` and "a .txt that parses as JSON"; the first would also hide a user's lab, survey or exam results, the second a note that quotes JSON. Neither is clearly safe, so the rule stays repo-only.

## 7. Eval plan

- **Regression tests.** `sr-core` `query::passages::tests::broad_question_reaches_the_core_file_within_the_budget` (the measured top 25 of §2.1: plain truncation to 9 / 15 misses lance.rs, the selection includes it, with at most 2 passages of the vendored file and no test file); `sr-engine` `answers::broad_question_reaches_the_core_file_within_the_budget` (a fixture folder with a shallow README, a provider file, two test suites and a core file with the implementation; a bag-of-words test embedder; the plain top 9 holds no core passage, the passages sent to the model do). Plus selection unit tests (lookups unchanged, tests prior and its off switch, same path in two folders, width bounds), grep shape tests, a `.seleneignore` scan test and chunker tests for the two fixtures.
- **Golden set.** `eval/golden/selene-retrieval.broad.jsonl`: the exact question (lance.rs and writer.rs grade 3; `index/mod.rs`, `docs/02-architecture.md` grade 2) and five more broad questions about this repo, over the same repo copy. The existing sets (`selene-src`, its exact identifiers, CQADupStack) stay as they are; selene-src's corpus is Selene's TypeScript and has no lance.rs.
- **Metrics.** Release bench (`sr-bench-engine`), same indexes before and after (DirectML, built once). Runs: `before` = the pre-change bench's search order; `after-search` = the new bench's search order (must be identical: the search is unchanged); `after-answer` = the new bench with `--first-stage answer`, i.e. exactly the passages an answer with that budget receives, at k = 10 (about the default first stage of 9), 15 (`maxSources` 20) and 50. Gate: no material regression (beyond noise) on selene-src (nDCG@10, R@5/10/50, MRR@10), its exact positives, and CQADupStack; the broad set should improve.

## 8. Results (2026-10-20)

### 8.1 Iteration on the eval (before the gate)

The pre-change release bench's wider candidate lists were cached and the selection replayed offline (a Python port of `select_passages`, then `xtask eval` on the replayed lists). nDCG@10 deltas in points vs the plain search at the same k:

| Variant | selene-src k 10 (file / chunk) | CQADupStack k 10 / 50 | Broad set k 10 | lance.rs in 9 |
|---|---|---|---|---|
| Bonus ½ / ¼, one passage per file per round (first design) | −0.6 / −4.2 | **−1.9 (CI −2.6 … −1.2) / −3.9** | +22.3 | 4th |
| No bonus, round robin | +0.6 / −4.2 | +0.1 / +0.0 | +11.3 | no |
| Bonus 0.1 / 0.05, round robin | +0.5 / −4.2 | −0.1 / −0.5 | +17.6 | 8th |
| **Bonus 0.1 / 0.05, soft repeat penalty 0.003 (shipped)** | **+0.1 / −0.8** | **−0.1 / −0.5** | **+14.8** | 9th (bench index) |
| Bonus 0.1 / 0.05, penalty 0.004, bonus only on a file's first pick | +0.5 / −2.5 | −0.1 / −0.5 | +15.4 | 8th |

The shipped variant has the smallest loss anywhere (chunk level included) while still lifting the broad set and getting core index files in. The replayed numbers equal the gate-built bench's numbers below to the third decimal.

### 8.2 The exact question, first stage before and after

Own daemon, same index (the owner's state: 501 of 524 files). Passages the model received (stub capture), by `maxSources`:

| `maxSources` (first stage) | Before (docs/06 build) | After, same index | After + repo `.seleneignore` (final) |
|---|---|---|---|
| 8 (6) | local_cpu.rs *(test)*, STL, v2_search.py, local.rs, schema.rs, verify_port.mjs | STL, v2_search.py, local.rs, schema.rs, embed.rs, verify_port.mjs | profiles.rs, v2_search.py, schema.rs, embed.rs, local.rs, verify_port.mjs |
| 12 (9, default) | + embed.rs, STL, STL: **no core index file** | + STL, profiles.rs, **writer.rs** | + **lexical.rs**, profiles.rs, **writer.rs** |
| 20 (15) | + tier_models.rs *(test)*, STL ×4, **writer.rs** | + **lance.rs (9th)**, writer.rs, lexical.rs, STL, events.rs, enhance.rs, policy.rs | + **lance.rs (10th)**, schema.rs, events.rs, enhance.rs, policy.rs, 04-roadmap.md |

(STL = `spikes/gpu-tier/stlshim/vector_algorithms.cpp`.) The plain `search` order is unchanged by design; with the repo rules lance.rs moves from 19th to 15th there. **Result:** the default answer now receives the index writer and the keyword index instead of a test file and three passages of vendored C++; lance.rs itself came in from `maxSources` 13 on **for this wording**. That did not carry over to the owner's own question ("How does Selene Retrieval's vector indexing engine work?"), which got no lance.rs passage at 20; see §9. It is not in the default 9 on this repo: `profiles.rs` (8 matching passages about embedding profiles and vector dimensions) and two eval scripts outrank it. A larger penalty would get it in (§8.1 last row, 8th) for a 1.7-point chunk-level cost; left as an owner choice (Amendment 14.6).

### 8.3 Eval, before vs after (release bench, gate build, same DirectML indexes)

`after-search` (the `search` order) equals `before` exactly on every set and k (0 wins, 0 losses), as designed. `after-answer` = the passages an answer receives:

| Set (queries) | k | Level | nDCG@10 | R@5 | R@10 | R@50 | MRR@10 | Δ nDCG@10 pts (95 % CI), W/T/L |
|---|---|---|---|---|---|---|---|---|
| selene-src (180) | 10 | file | 0.919 → 0.920 | 0.951 → 0.950 | 0.958 → 0.959 | | 0.923 → 0.921 | +0.09 (−0.71 … +0.81), 11/165/4 |
| | 10 | chunk | 0.825 → 0.817 | 0.859 → 0.844 | 0.899 → 0.887 | | 0.857 → 0.856 | |
| | 15 | file | 0.924 → 0.927 | 0.951 → 0.950 | 0.960 → 0.974 | | 0.923 → 0.921 | +0.36 (−0.57 … +1.24) |
| | 50 | file | 0.929 → 0.930 | 0.951 → 0.947 | 0.971 → 0.980 | 0.986 → 0.994 | 0.924 → 0.922 | +0.11 (−0.75 … +0.95) |
| | 50 | chunk | 0.825 → 0.815 | 0.859 → 0.842 | 0.899 → 0.882 | | 0.857 → 0.855 | |
| selene-src exact (23) | 10, 50 | file | 1.000 → 1.000 | 1.000 | 1.000 | 1.000 | 1.000 | 0/23/0 (lookups keep the fused order) |
| CQADupStack (506) | 10 | file | 0.362 → 0.360 | 0.379 → 0.377 | 0.451 → 0.453 | | 0.358 → 0.355 | −0.14 (−0.45 … +0.16), 4/492/10 |
| | 15 | file | 0.363 → 0.360 | 0.379 → 0.377 | 0.455 → 0.453 | | 0.358 → 0.355 | −0.27 (−0.63 … +0.08) |
| | 50 | file | 0.363 → 0.358 | 0.379 → 0.379 | 0.455 → 0.448 | 0.676 → 0.651 | 0.358 → 0.354 | −0.49 (−0.95 … −0.02) |
| Broad, this repo (6) | 10 | file | 0.233 → 0.381 | 0.236 → 0.325 | 0.236 → 0.408 | | 0.292 → 0.446 | **+14.77 (+6.11 … +22.94)**, 5/1/0 |
| | 15 | file | 0.267 → 0.408 | 0.236 → 0.325 | 0.278 → 0.450 | | 0.315 → 0.446 | +14.16 (+7.53 … +20.22) |
| | 50 | file | 0.310 → 0.401 | 0.236 → 0.269 | 0.375 → 0.442 | 0.614 → 0.656 | 0.315 → 0.446 | +9.05 (+0.30 … +17.91) |

Baselines match the milestone record (selene-src 0.929, CQADupStack 0.363 at k 50). At answer-sized budgets (k 10 and 15) every change on the existing sets is inside noise; the only interval that excludes zero is CQADupStack at k 50 (−0.5), a budget no answer uses (the largest first stage is 23, at `maxSources` 30). The chunk-level cost on selene-src (−0.8 to −1.0) is the price of not letting one file fill the budget; the strict per-file variants cost −4.2.

### 8.4 `grep`, skipped files, `.seleneignore`

- **`grep` tokens** (`o200k_base`, text + compact structured, same index): `merge_insert` (100 matches) **19,050 → 8,891** (−53 %); `IVF_HNSW_SQ` (20 matches) **3,806 → 1,932** (−49 %); one match 161 → 152.
- **Skipped / errored** (fresh index, new daemon): **503 of 524 indexed, 12 skipped, 9 couldn't be read** (was 501 / 12 / 11). `zalgo.txt` (2 chunks) and `bidi-zw.txt` (4 chunks) index now; the 9 left are the deliberately broken parser fixtures.
- **`.seleneignore`**: saving the new file into the indexed copy triggered the watcher's `settings_change` rescan; 21 files (`spikes/*/results/` and the STL copy) left the index without a restart (524 → 503 files).

### 8.5 Gate, release, installers

- **g1** (`-j 12`, MSVC 14.44): build; tests **832 passed / 0 failed / 7 ignored**; `cargo deny` ×3; sr-setup fmt, clippy and 40 tests; msi-ca 3 tests; release; sr-setup release; check-crt; check-delayload; check-bridge-deps; check-manifest (6 exes); `sync-providers --check`; UI ci, typecheck, lint, 61 tests and build: all green. **clippy** failed on one `manual_clamp` (`passage_width`) and **fmt** on three changed files. One fix batch (`clamp`, rustfmt), one clippy-only pass (clean).
- **g2** (the rerun): all green with the same counts (832 / 0 / 7, bindings test included).
- Process note: `msvc-env.sh` sets its own `R` and a spike `CARGO_TARGET_DIR`; the gate script also used `R`, so g1 and g2 built into `spikes/extract-sandbox/target` (deleted afterwards; the script now uses `P`). The checks ran on exactly the exes built from these sources.
- Release once (g2) and `dist` once: NSIS 124,049,435 B, NSIS offline 339,232,412 B, MSI 140,062,720 B, MSI offline 352,555,008 B (unsigned; sha256 in Amendment 14.7); check-manifest, check-crt and check-delayload pass on the staged exes. The previous set was moved to `installers/previous/` and deleted after the new set verified.

## 9. Follow-up: the owner's own wording (2026-10-20, installed Amendment 14 build)

The owner asked `answer` **"How does Selene Retrieval's vector indexing engine work?"** with `maxSources` 20 on a folder that is this project directory. The answer cited README, `docs/04-roadmap.md`, `crates/providers/src/local.rs` and `crates/engine/src/reembed.rs`, called the project "still in the planning stage" and said nothing about the Lance table, the ANN and scalar indexes or the query paths. §8.2 had measured a different wording ("How does vector indexing work?"); its "lance.rs from `maxSources` 13" did not hold for this one. The status also read "Indexing 482 of 505 files" with nothing queued.

### 9.1 Method

- A snapshot of this folder (the same `.seleneignore`; without `target/`, `installers/`, `vendor-bin/`, models, `node_modules`, the vendored MSVC and the installer build output) under `%TEMP%\sr46\selene-retrieval` (the directory name matters, see 9.3), indexed by the release `sr-bench-engine` with the bundled bge-small on DirectML into its own data dir. Same counts as the owner: **505 files**. The owner's install, pipe, port and data dir were not touched; no CLI or real model was called.
- The answer's first stage is `EngineHandle::answer_passages` (the function `answer` calls); `--first-stage answer` returns exactly those passages. The read round can only add excerpts of files already among them (S-09) and at most `maxSources − first` of them (5 at 20), and deeper rounds are off by default, so the first stage decides which files the model can see at all. The answer path itself (first stage, reads, numbering, the prompt the model receives) runs end to end with a scripted provider in `sr-engine` `answers::owner_question_gets_the_vector_store_and_a_query_path`.
- Variants were tried offline first: the pre-change bench's search lists (widths 30 / 45 / 150) for every golden query, the selection ported to Python (it reproduces the shipped numbers of §8.3 exactly: selene-src 0.920, CQADupStack 0.360 at k 10), rewritten queries searched by the same bench.

### 9.2 What the model received (first stage)

| # | Before (installed build), `maxSources` 20 → 15 | After, `maxSources` 20 → 15 |
|---|---|---|
| 1 | `crates/core/src/query/passages.rs:279` (a unit test quoting §2's question) | same |
| 2 | **`README.md:1-7`** | `docs/08-broad-query-ranking.md:1-9` |
| 3 | `passages.rs:1-22` | `crates/engine/src/reembed.rs:186-214` |
| 4 | `docs/08-broad-query-ranking.md:1-9` | **`crates/core/src/index/lance.rs:283-315`** (table and count cache) |
| 5 | `crates/engine/src/rerank.rs:13-35` | **`lance.rs:699-723`** |
| 6 | `eval/v2-baseline/v2_search.py:1-20` | `passages.rs:1-22` |
| 7 | `crates/engine/src/reembed.rs:49-74` | `reembed.rs:49-74` |
| 8 | **`CLAUDE.md:1-6`** | **`crates/core/src/index/lexical.rs:1140-1162`** (keyword lane) |
| 9 | `crates/providers/src/local.rs:52-76` | **`lance.rs:729-767`** (the vector query: ANN or exact scan by selectivity) |
| 10 | `reembed.rs:186-214` | **`crates/core/src/index/mod.rs:1-33`** (the store overview) |
| 11 | `crates/mcp-server/src/tools.rs:265-291` | `crates/engine/tests/answers.rs:458-481` |
| 12 | `eval/v2-baseline/v2_prep.mjs:1-17` | `crates/catalog/src/repo/profiles.rs:326-359` |
| 13 | `crates/engine/tests/answers.rs:458-481` | `crates/catalog/src/schema.rs:67-97` |
| 14 | **`docs/04-roadmap.md:13`** | **`crates/core/src/index/writer.rs:206-233`** |
| 15 | `tools.rs:894-911` | `reembed.rs:570-582` |

The default (`maxSources` 12 → 9) is rows 1–9 of each column: before, no core index file; after, three lance.rs passages and the keyword index. The before column contains all four files the owner's answer cited, so the reproduction matches; lance.rs was 23rd in the fused list, query/mod.rs 21st, nothing else of `crates/core/src/index/` in the top 45. The read round could only re-read one of these files (≤ 5 excerpts), so it could never reach the vector store.

- **Cited-only output.** `answer` lists the cited sources by default (`sources: "cited"`, docs/06 change 1); the model still saw all 15. Nothing useful was dropped by the output; the useful passages were never sent.
- **Planning docs.** Four of the 15 were documents (README, CLAUDE.md, docs/08, the roadmap), the rest engine-crate code, providers, MCP formatting, eval scripts and tests. The docs did not crowd the code out by number; the code that was sent was the wrong code, so the README and roadmap were the only description of the design the model had ("in outline", "planning stage").

### 9.3 Root cause

Two properties of the question, neither specific to this repository:

1. **The folder's own name.** "Selene Retrieval" is the folder's name. Inside that folder it says nothing about which passage answers, but it is the strongest match for every page that repeats it: the README title, CLAUDE.md, the roadmap, docs/08. Without the name (offline, same index) README and CLAUDE.md leave the 45 candidates and lance.rs moves from 23rd (2 passages) to 19th (9 passages), still outside the 15.
2. **Word forms.** The keyword lane matches whole words (no stemming). The question says "indexing"; the code says "index" (and the path `crates/core/src/index/` is boosted). So the keyword lane ranked `crates/engine/*` ("engine" in the path) and pages that literally say "indexing", and none of the vector store, while the dense lane had lance.rs 7th. Adding "index" alone (offline) put lance.rs at 7, 8 and 11, `index/mod.rs` at 17 and `writer.rs` at 23.

The passage selection of §4 was not at fault: given the second list it picks lance.rs ×3, the keyword index, the store overview and the writer.

### 9.4 Fix: the answer query (`sr_core::query::answer_query`, used by `EngineHandle::answer_passages`)

```text
answer (first search) and every deeper-round search:
  1. the question names a folder of the scope (display or directory name of ≥ 2 words, any case,
     separators or camelCase; "Retrieval's" counts)  and  ≥ 2 topic words remain without it
       → the name leaves the search text, and the search covers the named folder(s) only
  2. base forms of -ing / -ed words (≥ 5 letters, ASCII) are appended for the keyword lane:
       indexing → index, queried → query, embedded → embed, storing → stor, store
     (not for lookups or questions under 3 words)
  then: the search passage_width(budget) wide and select_passages, unchanged (§4)
```

- **Why narrowing, not just stripping.** With several folders in scope the name is what tells them apart; taking it out without narrowing would let another folder's notes compete (the regression test has a "Documents" folder whose notes match the question word for word: they are in the plain search, not in the answer). One-word names (`Documents`, `Notes`, `Taxes`) are never treated as a folder name: they are everyday words and often the topic itself.
- **Why only -ing / -ed.** Plurals were measured too (§9.5): they cost Q&A text more than they gain.
- **Considered, not done.** A file-kind (docs vs code) quota: the docs did not crowd the code out (9.2). Stronger file-evidence weights: +6 broad points for −0.6 to −1.1 on selene-src. A guaranteed slot for the top files by summed evidence: no effect (they were already in). Changing the `search` order itself: still out of scope (H, §3).
- `search`, `grep`, `read_file` and `enhance_prompt` rank as before; the plain search order is unchanged on every set (exact set 23/23 identical). No setting, nothing to configure (P1); scope can only shrink, reads still follow S-09.

### 9.5 Eval (release bench, same DirectML indexes; before = the shipped selection replayed over the unchanged search lists)

| Set (queries) | k | nDCG@10 | R@10 | R@50 | MRR@10 | Δ nDCG@10 pts (95 % CI), W/T/L |
|---|---|---|---|---|---|---|
| Broad, this repo (9: the six of §7 + the owner's question + 2 paraphrases) | 10 | 0.303 → 0.459 | 0.300 → 0.533 | | 0.406 → 0.528 | **+15.59 (+0.22 … +31.27)**, 5/3/1 |
| | 15 | 0.315 → 0.497 | 0.328 → 0.617 | | 0.414 → 0.528 | **+18.17 (+4.14 … +33.49)**, 5/3/1 |
| | 50 | 0.353 → 0.488 | 0.406 → 0.617 | 0.659 → 0.765 | 0.423 → 0.500 | +13.52 (+1.86 … +26.46), 5/3/1 |
| selene-src (180) | 10 | 0.920 → 0.916 | 0.959 → 0.959 | | 0.924 → 0.921 | −0.40 (−0.79 … −0.08), 1/172/7 |
| | 15 | 0.928 → 0.927 | 0.974 → 0.980 | | 0.924 → 0.921 | −0.07 (−0.52 … +0.42), 3/171/6 |
| | 50 | 0.930 → 0.928 | 0.980 → 0.980 | 0.994 → 0.992 | 0.925 → 0.921 | −0.24 (−0.59 … +0.04), 2/171/7 |
| selene-src exact (23) | 10, 50 | 1.000 → 1.000 | | | | 0/23/0 (lookups are left alone) |
| CQADupStack (506) | 10 | 0.360 → 0.360 | 0.453 → 0.454 | | 0.355 → 0.354 | −0.04 (−0.77 … +0.75), 25/457/24 |
| | 15 | 0.360 → 0.359 | 0.453 → 0.454 | | 0.355 → 0.353 | −0.12 (−0.83 … +0.66), 24/458/24 |
| | 50 | 0.358 → 0.358 | 0.448 → 0.451 | 0.651 → 0.648 | 0.354 → 0.352 | −0.03 (−0.75 … +0.76), 26/458/22 |

The owner's question alone (B007): lance.rs, the keyword index, `index/mod.rs` and `writer.rs` in the 15; lance.rs ×3 and the keyword index in the default 9. Offline variants (same harness, nDCG@10 Δ at k 10 / 15): name only +3.7 / +3.7 on broad, 0 elsewhere (no eval folder name occurs in its questions); name + all word forms (plurals too) +14.4 / +18.3 broad, −0.44 / −0.43 selene-src, **−0.51 / −0.53 CQADupStack**; name + -ing only +18.3 / +19.4 broad, −0.28 / +0.01 selene-src, −0.35 / −0.35 CQADupStack; **name + -ing / -ed (shipped)** as in the table. Note: Windows Defender quarantined 4 CQADupStack documents of the bench corpus (owner report, same day); both columns use the same index, so the deltas hold, the absolute numbers are slightly low.

Tests: `sr-core` `query::passages::tests::owner_question_reaches_the_vector_store_and_a_query_path` (the measured top 25 of both lists: as asked, the 15 hold README, CLAUDE.md and the roadmap and no lance.rs; with the answer query lance.rs and a query-path file at 9 and 15, lance.rs's table and query passages, the store overview), `the_folder_name_leaves_the_question_and_verbs_get_base_forms`, `base_forms_of_ing_and_ed_words`; `sr-engine` `answers::owner_question_gets_the_vector_store_and_a_query_path` (the real answer path with a scripted provider at `maxSources` 20: lance.rs and a query-path file among the passages sent, nothing from the other folder, no `\r` in the prompt or the sources, line numbers of a CRLF file exact).

### 9.6 "Indexing 482 of 505 files" with nothing queued

Not stuck files: a counter. 505 − 482 = 23 = **12 skipped on purpose** (10 JSON vector dumps `data_file`: `crates/providers/tests/fixtures/bge_small_ref_st.json` and 9 under `spikes/native-stack/test11/`; `spikes/extract-sandbox/fixtures/long-line.txt` and `oversize.txt` `too_large`) + **11 that couldn't be read** (`files_skipped` = `dead`: the 9 deliberately broken parser fixtures of §5, plus `zalgo.txt` and `bidi-zw.txt`, which failed before the §5 chunker fix and stay dead until retried, because their content and chunk settings did not change; a fresh index of the same folder has 9 dead and indexes both). `EngineStatus::plain_summary` said "Indexing {indexed} of {total}" whenever indexed < total, summed over every folder, so any folder with a skipped or unreadable file read as indexing for ever (the MCP `status` summary; the app derives its own line from folder states). **Fix:** only folders that are pending, scanning or indexing count, and in those a file that couldn't be read counts as done. A settled folder reads "Up to date" (its line still says "some files couldn't be read"). Test: `sr-common` `events::tests::settled_folders_with_skipped_files_never_read_as_indexing`.

### 9.7 Line ends

Passage text sent to MCP clients and models had every `\r` of a Windows file (one wasted token per line). `search` snippets (and so `answer` sources, the answer prompt and `enhance_prompt` hits) now turn `\r\n` and a lone `\r` into `\n` (`sr_engine::files::normalize_newlines`); `read_file` and `grep`, which print one numbered file line per output line, drop the line end and show a lone `\r` inside a line as a space (`one_line`), so one output line stays one file line. Line numbers are unchanged: they come from the index and `read_file`, which count `\n` only. Tests: `sr-engine` `files::tests::line_ends_are_normalised_without_moving_line_numbers` and the CRLF files of the answer test above.

### 9.8 Gate

g1 (`-j 12`, MSVC 14.44): UI `npm ci`, typecheck, lint, build, tests; `cargo fmt --check`; build; **842 tests passed / 0 failed / 7 ignored** (bindings test included); clippy `-D warnings`; `cargo deny --locked` ×3; sr-setup fmt, clippy and tests; msi-ca tests; release; sr-setup release; check-crt; check-delayload; check-bridge-deps; check-manifest; `sync-providers --check`: **all green on the first run**, so no fix batch and no rerun were needed.

Release once (in g1), `dist` once, the four installers (unsigned; check-crt, check-delayload and check-manifest pass on the staged exes): NSIS 124,077,406 B (sha256 7a138554…aea6d956), NSIS offline 339,239,300 B (8a72c3cf…4b6605cc), MSI 140,079,104 B (00608141…0a5f109d), MSI offline 352,571,392 B (5155beff…e554fa10). Full hashes: `target/package/SHA256SUMS*.txt`. The previous set was moved to `installers/previous/` and deleted after the new set verified; no `spikes/*/target` left.
