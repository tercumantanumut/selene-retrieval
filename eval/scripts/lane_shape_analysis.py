"""Exact-lane query-shape analysis (M5-E, R-UX37).

Reads an xtask eval `--json` report with two runs (lane on / lane off) and the golden set, computes a
Python approximation of sr-core's `QueryShape` (identifier-like words, numbers, compounds), and
prints per-shape nDCG@10 deltas, so a lane gate can be chosen from data instead of guessed.

    python eval/scripts/lane_shape_analysis.py <report.json> <golden.jsonl> [on_label] [off_label]
"""
import json
import re
import sys
from collections import defaultdict


def identifier_like(w: str) -> bool:
    t = w.strip("_")
    if not t:
        return False
    if any(c.isdigit() for c in t) or "_" in t:
        return True
    for a, b in zip(t, t[1:]):
        if a.islower() and b.isupper():
            return True
    for a, b, c in zip(t, t[1:], t[2:]):
        if a.isupper() and b.isupper() and c.islower():
            return True
    return False


WORD = re.compile(r"[\w]+", re.UNICODE)


def shape(q: str) -> dict:
    words = [w.strip("_") for w in WORD.findall(q) if w.strip("_")]
    ident = [w for w in words if identifier_like(w) and not (w.isdigit() and len(w) < 2)]
    # compounds joined by . - : with a digit, e.g. v1.2, ERR-1234, 2026-10-19
    comps = re.findall(r"\w+(?:(?:::|[.\-:])\w+)+", q)
    comps = [c for c in comps if any(ch.isdigit() for ch in c)]
    digits = [w for w in words if w.isdigit()]
    long_digits = [w for w in digits if len(w) >= 4]
    strong = [
        w
        for w in ident
        if ("_" in w)
        or (any(c.isdigit() for c in w) and any(c.isalpha() for c in w))
        or (w.isdigit() and len(w) >= 4)
    ]
    return {
        "words": len(words),
        "ident": ident,
        "comps": comps,
        "digits": digits,
        "long_digits": long_digits,
        "strong": strong,
    }


def main() -> None:
    rep = json.load(open(sys.argv[1], encoding="utf-8"))
    golden = {}
    for line in open(sys.argv[2], encoding="utf-8"):
        d = json.loads(line)
        golden[d["qid"]] = d["query"]
    on_label = sys.argv[3] if len(sys.argv) > 3 else "exact-on"
    off_label = sys.argv[4] if len(sys.argv) > 4 else "exact-off"
    runs = {r["label"]: {q["qid"]: q["file"]["ndcg10"] for q in r["queries"]} for r in rep["runs"]}
    on, off = runs[on_label], runs[off_label]
    groups = defaultdict(list)
    detail = []
    for qid, q in golden.items():
        if qid not in on or qid not in off:
            continue
        s = shape(q)
        if not s["ident"] and not s["comps"]:
            g = "no-exact-terms"
        elif s["strong"] or s["comps"]:
            g = "strong-terms" + ("-short" if s["words"] <= 3 else "-sentence")
        else:
            g = "weak-terms" + ("-short" if s["words"] <= 3 else "-sentence")
        d = on[qid] - off[qid]
        groups[g].append(d)
        if abs(d) > 1e-9 and g != "no-exact-terms":
            detail.append((d, g, qid, q, s["ident"]))
    print(f"{'group':28} {'n':>4} {'mean d(on-off)':>15} {'wins':>5} {'losses':>6}")
    for g, ds in sorted(groups.items()):
        n = len(ds)
        print(
            f"{g:28} {n:4d} {sum(ds) / n:15.4f} {sum(1 for x in ds if x > 1e-9):5d} {sum(1 for x in ds if x < -1e-9):6d}"
        )
    tot = [x for ds in groups.values() for x in ds]
    print(f"{'ALL':28} {len(tot):4d} {sum(tot) / len(tot):15.4f}")
    if "-v" in sys.argv:
        for d, g, qid, q, ident in sorted(detail):
            print(f"{d:+.3f} {g:22} {qid:6} {q!r} {ident}")


if __name__ == "__main__":
    main()
