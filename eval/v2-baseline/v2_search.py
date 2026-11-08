"""Selene V2 query-side ranking, re-implemented offline (faithful port of
lib/vectordb/v2/hybrid-search.ts + lib/ai/vector-search/v2/fusion.ts + lib/vectordb/search.ts at
Selene commit e098025, effective fresh-install settings):

  topK = 50 (vectorSearch tool default maxResults), candidateLimit = topK * 2 = 100
  for each expanded query (query-expansion on):        dense  : exact cosine (LanceDB flat scan,
                                                                no ANN index is ever built), keep
                                                                limit 100 then score >= 0.01
                                                       lexical: cosine of 4096-d hashed TF vectors
  mergeHitsByScore (max per chunk id, sort desc, slice 100) per arm
  dense empty -> lexical[:50]; lexical empty -> dense[:50]
  RRF: score(id) = 1.0/(50 + i_dense) + 2.0/(50 + i_lex)  (0-based ranks), stable sort, top 50
  reranking off.

Assumptions (documented): a lexical vector with no tokens (all stopwords) is a zero vector; cosine is
undefined (NaN in LanceDB), so we treat it as "no lexical hits" and zero-vector chunks never match.
`--zero-fill` selects how LanceDB's always-`limit`-rows behaviour is modelled (see --help).

Output: harness results JSON (eval/README.md), one result per line. `latency_ms` is null: this
script's timings are not Selene's latency.

  python v2_search.py <chunks.jsonl> <emb.npy> <queries.prepped.jsonl> <qemb.npy> <out.json>
         --variant <label> --corpus <name>
"""
import argparse, json, time
from collections import defaultdict

import numpy as np

TOP_K, RRF_K, DENSE_W, LEX_W, DENSE_MIN = 50, 50, 1.0, 2.0, 0.01


def write_results(path, header, results):
    """Harness results JSON, compact, one result per line (diff-friendly, ~half the size)."""
    head = json.dumps(header, separators=(",", ":"))[:-1]
    with open(path, "w", encoding="utf-8", newline="\n") as fh:
        fh.write(head + ',"results":[\n')
        fh.write(",\n".join(json.dumps(r, separators=(",", ":")) for r in results))
        fh.write("\n]}\n")


def load_chunks(path):
    meta, postings = [], defaultdict(lambda: ([], []))
    with open(path, encoding="utf-8") as fh:
        for i, line in enumerate(fh):
            c = json.loads(line)
            meta.append((c["doc"], c["start_line"], c["end_line"], c["id"]))
            for b, v in c["lex"].items():
                postings[int(b)][0].append(i)
                postings[int(b)][1].append(v)
    post = {b: (np.array(ix, dtype=np.int64), np.array(vs, dtype=np.float64)) for b, (ix, vs) in postings.items()}
    return meta, post


def topn(scores, n):
    if len(scores) <= n:
        idx = np.argsort(-scores, kind="stable")
    else:
        part = np.argpartition(-scores, n)[:n]
        idx = part[np.argsort(-scores[part], kind="stable")]
    return idx


def merge_by_score(lists, n):
    best = {}
    for lst in lists:
        for i, s in lst:
            if i not in best or s > best[i]:
                best[i] = s
    return sorted(best.items(), key=lambda kv: -kv[1])[:n]


def main():
    ap = argparse.ArgumentParser()
    for a in ("chunks", "emb", "queries", "qemb", "out"):
        ap.add_argument(a)
    ap.add_argument("--variant", required=True)
    ap.add_argument("--corpus", required=True)
    ap.add_argument("--zero-fill", choices=["none", "rowid"], default="none",
                    help="none: drop zero-similarity lexical rows (conservative V2, gate baseline); "
                         "rowid: pad to the candidate limit like LanceDB's flat kNN (as shipped)")
    ap.add_argument("--dump-candidates", help="JSONL of per-query dense/lexical candidate lists "
                    "(input to verify_port.mjs, which re-fuses them with Selene's own fusion.ts)")
    ap.add_argument("--mode", choices=["hybrid", "dense", "lexical"], default="hybrid",
                    help="ablation: 'dense' = V1-style semantic only (searchVectorDB with V2 chunks); "
                         "'lexical' = hashed lexical arm only. Not Selene configurations.")
    a = ap.parse_args()

    meta, post = load_chunks(a.chunks)
    has_lex = np.zeros(len(meta), dtype=bool)
    for ix, _ in post.values():
        has_lex[ix] = True
    E = np.load(a.emb)
    QE = np.load(a.qemb)
    n = len(meta)
    assert E.shape[0] == n, (E.shape, n)
    cand = TOP_K * 2
    results, qrow, padded_queries = [], 0, 0
    dump = open(a.dump_candidates, "w", encoding="utf-8", newline="\n") if a.dump_candidates else None
    with open(a.queries, encoding="utf-8") as fh:
        queries = [json.loads(l) for l in fh if l.strip()]
    for q in queries:
        t0 = time.perf_counter()
        dense_lists, lex_lists = [], []
        for e in q["expanded"]:
            qv = QE[qrow]
            qrow += 1
            sims = E @ qv
            idx = topn(sims, cand)
            dense_lists.append([(int(i), float(sims[i])) for i in idx if sims[i] >= DENSE_MIN])
            if e["lex"]:
                acc = np.zeros(n, dtype=np.float64)
                for b, qvv in e["lex"].items():
                    p = post.get(int(b))
                    if p is not None:
                        acc[p[0]] += p[1] * qvv
                pos = np.nonzero(acc > 0)[0]
                ranked = pos[np.argsort(-acc[pos], kind="stable")][:cand]
                hits = [(int(i), float(acc[i])) for i in ranked]
                if len(hits) < cand:
                    padded_queries += 1
                if a.zero_fill == "rowid" and len(hits) < cand:
                    # LanceDB returns `limit` rows even at cosine 0; Selene never filters the lexical
                    # arm, so V2 pads with zero-similarity rows (tie order modelled as row order).
                    need = cand - len(hits)
                    pad = np.nonzero((acc <= 0) & has_lex)[0][:need]
                    hits.extend((int(i), 0.0) for i in pad)
                lex_lists.append(hits)
        dense = merge_by_score(dense_lists, cand) if a.mode != "lexical" else []
        lex = merge_by_score(lex_lists, cand) if a.mode != "dense" else []
        if not dense:
            fused = [(i, s) for i, s in lex[:TOP_K]]
        elif not lex:
            fused = [(i, s) for i, s in dense[:TOP_K]]
        else:
            scores = {}
            for r, (i, _) in enumerate(dense):
                scores[i] = scores.get(i, 0.0) + DENSE_W / (RRF_K + r)
            for r, (i, _) in enumerate(lex):
                scores[i] = scores.get(i, 0.0) + LEX_W / (RRF_K + r)
            fused = sorted(scores.items(), key=lambda kv: -kv[1])[:TOP_K]
        el = (time.perf_counter() - t0) * 1000
        if dump:
            def ids(lst):
                return [[meta[i][3], s] for i, s in lst]
            dump.write(json.dumps({"qid": q["qid"], "dense": ids(dense), "lex": ids(lex),
                                   "fused": [meta[i][3] for i, _ in fused]}) + "\n")
        del el  # this script's timing is not Selene's; latency_ms stays null
        results.append({
            "qid": q["qid"], "latency_ms": None,
            "hits": [{"path": meta[i][0], "start_line": meta[i][1], "end_line": meta[i][2],
                      "score": round(s, 6)} for i, s in fused],
        })
    assert qrow == QE.shape[0], (qrow, QE.shape)
    out = {
        "engine": "selene-v2-reimpl",
        "variant": a.variant,
        "corpus": a.corpus,
        "baseline_kind": "faithful offline re-implementation of Selene V2 hybrid search",
        "params": {"top_k": TOP_K, "candidate_limit": cand, "rrf_k": RRF_K, "dense_weight": DENSE_W,
                   "lexical_weight": LEX_W, "dense_min_score": DENSE_MIN, "query_expansion": True,
                   "rerank": False, "chunking": "o200k 96/48 tokens", "selene_commit": "e098025",
                   "lexical_zero_fill": a.zero_fill, "mode": a.mode,
                   "lexical_lists_shorter_than_candidate_limit": padded_queries},
    }
    write_results(a.out, out, results)
    if dump:
        dump.close()
    print(f"{len(results)} queries -> {a.out} (lexical lists < {cand}: {padded_queries})")


if __name__ == "__main__":
    main()
