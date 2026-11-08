# Golden sets

Each file holds one query per line, with the files and line ranges that answer it. `cargo xtask eval` scores search results against them. The [eval README](../README.md) explains the corpora, the baselines and how to run an evaluation.

## Line format

```json
{"qid": "I001", "query": "rrfFusion", "kind": "identifier", "relevant": [{"path": "lib/ai/vector-search/v2/fusion.ts", "grade": 3, "spans": [[21, 43]], "label": "sym:rrfFusion"}]}
```

- `grade`: 3 answers the query, 2 is strongly related, 1 is a marginal mention. Recall@k and MRR count grade 2 and above. Grade 1 adds only partial gain to nDCG.
- `spans`: 1-based, inclusive line ranges. An empty list means the whole file or document.
- `label`: where the unit came from (for example `sym:` for a definition, `exact:` for an exact match). The scorer ignores it.
- Negative sets have `"kind": "negative"` and an empty `relevant` list: the right result is "nothing closely matched".

## Files

| File | What it holds | Made by |
|---|---|---|
| `src/selene-src.spec.jsonl` | The hand-written spec for `selene-src.jsonl`: queries and labels by symbol, file or line range | written by hand |
| `selene-src.jsonl` | 180 identifier, path and natural-language queries over the `selene-src` corpus | `eval/scripts/build_golden.py` from the spec. Definition spans come from `eval/scripts/extract_symbols.py`; identifier queries also get every other file that mentions the identifier, at grade 1 |
| `selene-src.exact.jsonl` | 23 exact identifiers and numbers, each with every line it occurs on | `eval/scripts/build_exact_golden.py` |
| `selene-src.exact-negatives.jsonl` | 16 identifiers and numbers that occur nowhere in the corpus | `eval/scripts/build_exact_golden.py` (it checks that they occur nowhere) |
| `selene-src.negatives.jsonl` | 20 queries with no answer in the corpus, used to calibrate the "no good match" cut-off (`eval/scripts/calibrate_no_match.py`) | written by hand |
| `selene-retrieval.broad.jsonl` | 9 broad "how does X work" questions over this repository | written by hand |
| `cqadupstack-webmasters.jsonl` | Question titles and duplicate-question labels from CQADupStack webmasters. The source labels are yes or no, so every relevant document is grade 3 | `eval/scripts/fetch_cqadupstack.py` |

`eval/scripts/extract_symbols.py` only proposes candidate labels. Every committed label was reviewed by hand. A manual sample check of the definition spans found two that the simple lexer got wrong; `eval/scripts/build_golden.py` corrects them (`SPAN_OVERRIDES`).

## Licenses

- The `selene-src` paths, line numbers and identifiers come from [Selene](https://github.com/tercumantanumut/selene) at commit `e098025`, MIT License, Copyright (c) 2026 tercumantanumut.
- `cqadupstack-webmasters.jsonl` contains question titles written by the post authors on webmasters.stackexchange.com, under [CC BY-SA 3.0](https://creativecommons.org/licenses/by-sa/3.0/) (posts written before April 2011: CC BY-SA 2.5). The source and citations are in the [corpus README](../corpora/cqadupstack-webmasters/README.md). The BEIR and MTEB packaging is Apache-2.0.
