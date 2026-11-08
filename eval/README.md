# Retrieval evaluations

These evaluations measure how well Selene Retrieval finds the right files and passages. Each golden set lists queries with the files (and line ranges) that answer them. `cargo xtask eval` runs one or more engines over a golden set and reports recall@5, @10 and @50, nDCG@10, MRR@10 and latency (p50, p95). It compares every engine with the first one (the baseline), with a paired-bootstrap 95% confidence interval, and can fail when quality drops (`--max-ndcg-loss`) or does not improve (`--min-ndcg-delta`).

You don't need any of this to build or test the project. The evaluations need Python, Node.js and a Selene checkout.

## Corpora

- `corpora/selene-src/`: the git-tracked source files of the public [Selene](https://github.com/tercumantanumut/selene) repository at commit `e098025` (`e09802592ccb61e1764e1f0c2c1ffbb3e8de25d4`). Only the manifest (paths, sizes, sha256) is committed. `scripts/build_selene_corpus.py` copies the files from the Selene checkout next to this repository (`../selene`) into `eval/.cache/`.
- `corpora/cqadupstack-webmasters/`: web-development questions from the Webmasters Stack Exchange (CQADupStack, BEIR packaging), pinned to one revision. `scripts/fetch_cqadupstack.py` downloads it into `eval/.cache/` and checks each file's sha256. Its [README](corpora/cqadupstack-webmasters/README.md) has the source, citations and license.

`eval/.cache/` holds downloaded corpora, models and embeddings. It is never committed.

## Golden sets (`golden/`)

The line format, the grades and the licenses are in [golden/README.md](golden/README.md).

| File | What it holds | Made by |
|---|---|---|
| `selene-src.jsonl` | Identifier, path and natural-language queries over `selene-src`, graded 1 to 3 | `scripts/build_golden.py`, from the hand-written spec `golden/src/selene-src.spec.jsonl` |
| `selene-src.exact.jsonl`, `selene-src.exact-negatives.jsonl` | Exact identifiers and numbers that must be found, and identifiers and numbers that occur nowhere in the corpus, which must find nothing | `scripts/build_exact_golden.py` |
| `selene-src.negatives.jsonl` | Queries with no answer in the corpus (for the "no good match" cut-off) | committed as is (no generator script) |
| `selene-retrieval.broad.jsonl` | Broad "how does X work" questions over this repository | committed as is (no generator script) |
| `cqadupstack-webmasters.jsonl` | Question titles and duplicate-question labels | `scripts/fetch_cqadupstack.py` |

`scripts/extract_symbols.py` proposes candidate labels for `selene-src`; every committed label was reviewed by hand.

## Baselines (`baselines/`)

The baselines are Selene V2's search results, from an offline re-implementation of its indexing and ranking in `v2-baseline/`. There is one file per corpus and model variant (`large-fp32`, `small-fp32`, `small-q8`). `baselines/ablations/` holds dense-only, keyword-only and reference-dense runs.

Reproduce them with:

```text
eval/v2-baseline/run_baseline.sh <selene-src|cqadupstack-webmasters> [large-fp32 small-fp32 small-q8]
```

It needs Node.js with Selene's installed `node_modules`, a Python environment in `eval/.cache/venv` with `onnxruntime-directml`, `numpy` and `tokenizers`, and the models in `eval/.cache/models`. Set `EMBED_PROVIDER=cpu` to run without DirectML. `v2-baseline/verify_port.mjs` and `verify_embed.mjs` check the re-implementation against Selene's own code. The `.mjs` scripts read Selene from `../selene`, or from `SELENE_ROOT` if set.

## Running Selene Retrieval against a golden set

`sr-bench-engine` (in `crates/engine`) indexes a corpus with the real engine and answers queries on stdin. `--app-dir crates/providers` points it at the model that `cargo xtask fetch-models` downloads. For example:

```text
cargo xtask eval --golden eval/golden/selene-src.jsonl \
  --engine v2=results:eval/baselines/selene-src.selene-v2.large-fp32.json \
  --engine "ours=cmd:<path to sr-bench-engine.exe> --index <index dir> --corpus-dir eval/.cache/corpora/selene-src --app-dir crates/providers"
```

Other scripts in `scripts/`:

- `m5e_eval.sh`: the ranking runs used for the later milestones (exact-match lane, reranker). Its default paths expect a local build layout; set `BENCH`, `XTASK`, `SRC_IDX` and `CQA_IDX` to your own.
- `calibrate_no_match.py`: calibrates the "no good match" cut-off from positive and negative queries.
- `lane_shape_analysis.py`: compares an eval report with the exact-match lane on and off, by query shape.
- `reference_dense.py`: a textbook BGE dense-search engine, to sanity-check the harness.

`enhance/build_fixtures.mjs` runs Selene's own prompt enhancer on fixed inputs and writes `enhance/fixtures.json`. The prompt-enhancer tests in `crates/core` check that Selene Retrieval sends the same request text.

## Data licenses

- `selene-src` paths and line numbers come from Selene, MIT License, Copyright (c) 2026 tercumantanumut.
- The CQADupStack text is Stack Exchange user content under [CC BY-SA 3.0](https://creativecommons.org/licenses/by-sa/3.0/) (posts written before April 2011: CC BY-SA 2.5), by the post authors on webmasters.stackexchange.com. `golden/cqadupstack-webmasters.jsonl` keeps only question titles and labels, with attribution as in the [corpus README](corpora/cqadupstack-webmasters/README.md). The BEIR and MTEB packaging is Apache-2.0.
