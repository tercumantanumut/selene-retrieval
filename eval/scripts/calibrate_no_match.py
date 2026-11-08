"""Calibrate the "no good match" cut-off (R-UX18, sr_core::query::NoMatchCutoff).

Runs `sr-bench-engine --cutoff off` over the positive golden queries and the negative set (queries
with no answer), records each query's best dense cosine (`top_dense`) and best BM25 score
(`top_bm25`), and reports, for a grid of (dense floor, BM25 floor) pairs, how many negatives the
rule would empty and how much file-level nDCG@10 the positives would lose (a cut positive scores
0). The per-query nDCG comes from an `xtask eval --json` report of the same engine without the
cut-off.

    python eval/scripts/calibrate_no_match.py --bench <sr-bench-engine.exe> --bench-args "..." \
        --golden eval/golden/selene-src.jsonl --negatives eval/golden/selene-src.negatives.jsonl \
        --eval-json <report.json> --label <run label> [--max-loss 0.5] [--out signals.jsonl]

The rule fires when top_dense < dense_floor AND top_bm25 < bm25_floor (no BM25 hit = 0).
"""

import argparse
import json
import shlex
import subprocess
import sys


def load(path):
    with open(path, encoding="utf-8") as f:
        return [json.loads(l) for l in f if l.strip()]


def run_bench(bench, bench_args, queries):
    cmd = [bench] + shlex.split(bench_args, posix=False) + ["--cutoff", "off"]
    p = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True,
                         encoding="utf-8")
    out = {}
    for q in queries:
        p.stdin.write(json.dumps({"qid": q["qid"], "query": q["query"], "k": 50}) + "\n")
        p.stdin.flush()
        r = json.loads(p.stdout.readline())
        if "error" in r:
            sys.exit(f"{q['qid']}: {r['error']}")
        out[q["qid"]] = {
            "qid": q["qid"],
            "query": q["query"],
            "kind": q.get("kind", ""),
            "top_dense": r.get("top_dense") or 0.0,
            "top_bm25": r.get("top_bm25") or 0.0,
            "n_hits": len(r.get("hits", [])),
        }
    p.stdin.close()
    p.wait(timeout=120)
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bench", required=True)
    ap.add_argument("--bench-args", required=True)
    ap.add_argument("--golden", required=True)
    ap.add_argument("--negatives", required=True)
    ap.add_argument("--eval-json", required=True)
    ap.add_argument("--label", required=True, help="run label in the eval json")
    ap.add_argument("--max-loss", type=float, default=0.5, help="nDCG points")
    ap.add_argument("--out")
    a = ap.parse_args()

    pos = load(a.golden)
    neg = load(a.negatives)
    rep = json.load(open(a.eval_json, encoding="utf-8"))
    run = next(r for r in rep["runs"] if r["label"] == a.label)
    ndcg = {q["qid"]: q["file"]["ndcg10"] for q in run["queries"]}
    sig = run_bench(a.bench, a.bench_args, pos + neg)
    if a.out:
        with open(a.out, "w", encoding="utf-8") as f:
            for q in pos + neg:
                s = dict(sig[q["qid"]])
                s["ndcg10"] = ndcg.get(q["qid"])
                s["negative"] = q in neg
                f.write(json.dumps(s) + "\n")

    def stats(rows):
        d = sorted(r["top_dense"] for r in rows)
        b = sorted(r["top_bm25"] for r in rows)
        return d, b

    pd, pb = stats([sig[q["qid"]] for q in pos])
    nd, nb = stats([sig[q["qid"]] for q in neg])
    print(f"positives: n={len(pos)} top_dense min {pd[0]:.3f} p5 {pd[len(pd)//20]:.3f} "
          f"median {pd[len(pd)//2]:.3f}; top_bm25 min {pb[0]:.2f} median {pb[len(pb)//2]:.2f}")
    print(f"negatives: n={len(neg)} top_dense max {nd[-1]:.3f} median {nd[len(nd)//2]:.3f}; "
          f"top_bm25 max {nb[-1]:.2f} median {nb[len(nb)//2]:.2f}")
    print()
    print("negatives (sorted by top_dense):")
    for q in sorted(neg, key=lambda q: sig[q["qid"]]["top_dense"]):
        s = sig[q["qid"]]
        print(f"  {q['qid']} dense {s['top_dense']:.3f} bm25 {s['top_bm25']:6.2f}  {q['query']}")
    print("lowest positives by top_dense:")
    for q in sorted(pos, key=lambda q: sig[q["qid"]]["top_dense"])[:25]:
        s = sig[q["qid"]]
        print(f"  {q['qid']} dense {s['top_dense']:.3f} bm25 {s['top_bm25']:6.2f} "
              f"ndcg {ndcg.get(q['qid'], 0):.3f}  {q['query']}")
    print()
    n_pos = len(pos)
    best = None
    print("dense_floor bm25_floor  neg_cut  pos_cut  ndcg_loss_pts")
    for df in [x / 100 for x in range(40, 86, 1)]:
        for bf in [0, 2, 4, 6, 8, 10, 12, 15, 20, 25, 30, 40]:
            fires = lambda s: s["top_dense"] < df and s["top_bm25"] < bf
            neg_cut = sum(1 for q in neg if fires(sig[q["qid"]]))
            cut = [q["qid"] for q in pos if fires(sig[q["qid"]])]
            loss = 100.0 * sum(ndcg.get(qid, 0.0) for qid in cut) / n_pos
            if loss <= a.max_loss:
                key = (neg_cut, -len(cut), -loss)
                if best is None or key > best[0]:
                    best = (key, df, bf, neg_cut, cut, loss)
            if bf in (0, 10, 20) and int(round(df * 100)) % 5 == 0:
                print(f"   {df:.2f}       {bf:5.1f}      {neg_cut:3d}      {len(cut):3d}     {loss:6.3f}")
    print()
    if best:
        _, df, bf, nc, cut, loss = best
        print(f"BEST within {a.max_loss} pt: dense_floor {df:.2f} bm25_floor {bf} -> negatives cut "
              f"{nc}/{len(neg)}, positives cut {len(cut)} {cut}, nDCG loss {loss:.3f} pt")


if __name__ == "__main__":
    main()
