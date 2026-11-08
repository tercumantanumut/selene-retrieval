"""Resolve the hand-written golden spec (eval/golden/src/selene-src.spec.jsonl) into the committed,
engine-agnostic golden file eval/golden/selene-src.jsonl.

Spec label forms (grade 3 = answers the query, 2 = strongly related, 1 = marginal/mention):
  ["sym",  "name" | "path::name", g]  -> definition span from extract_symbols.py (doc comment incl.)
  ["file", "path", g]                  -> whole file
  ["span", "path:a-b", g]              -> explicit 1-based inclusive line range
  "refs": true (identifier queries)    -> every other file containing the identifier as a word is
                                          added at grade 1 with its occurrence lines (+-2), max 15
                                          files by occurrence count. Explicit labels win.

Output line: {"qid","query","kind","relevant":[{"path","grade","spans":[[a,b],..]}, ...]}
Fails loudly on unknown/ambiguous symbols, unknown paths and out-of-range spans.

Usage: python eval/scripts/build_golden.py  (needs eval/.cache/symbols.json; run extract_symbols.py)
"""
import json, os, re, sys
from collections import defaultdict

EVAL = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ROOT = os.path.join(EVAL, ".cache", "corpora", "selene-src")
SPEC = os.path.join(EVAL, "golden", "src", "selene-src.spec.jsonl")
OUT = os.path.join(EVAL, "golden", "selene-src.jsonl")
REF_CAP, REF_CTX = 15, 2
# Hand-verified corrections where the crude lexer in extract_symbols.py mis-spans a declaration
# (regex literals containing quotes/backticks). Found by the manual sample check (eval/golden/README.md).
SPAN_OVERRIDES = {
    "lib/vectordb/v2/lexical-vectors.ts::tokenizeForLex": [[22, 32]],
    "lib/vectordb/v2/lexical-vectors.ts::generateLexicalVector": [[45, 66]],
}


def main():
    man = json.load(open(os.path.join(EVAL, "corpora", "selene-src", "manifest.json"), encoding="utf-8"))
    lines = {f["path"]: f["lines"] for f in man["files"]}
    syms = json.load(open(os.path.join(EVAL, ".cache", "symbols.json"), encoding="utf-8"))
    by_name = defaultdict(list)
    for s in syms:
        by_name[s["name"]].append(s)
    texts = {}

    def text(p):
        if p not in texts:
            texts[p] = open(os.path.join(ROOT, p), encoding="utf-8", errors="replace").read().replace("\r\n", "\n").split("\n")
        return texts[p]

    errors, out, qids = [], [], set()
    for ln, raw in enumerate(open(SPEC, encoding="utf-8"), 1):
        if not raw.strip():
            continue
        q = json.loads(raw)
        if q["qid"] in qids:
            errors.append(f"{q['qid']}: duplicate qid")
        qids.add(q["qid"])
        rel = []
        for kind, ref, grade in q["rel"]:
            if kind == "sym":
                path, name = ref.split("::") if "::" in ref else (None, ref)
                cands = [s for s in by_name.get(name, []) if path is None or s["path"] == path]
                files = {s["path"] for s in cands}
                if not cands:
                    errors.append(f"{q['qid']}: unknown symbol {ref}")
                    continue
                if len(files) > 1:
                    errors.append(f"{q['qid']}: ambiguous symbol {ref}: {sorted(files)}")
                    continue
                spans = SPAN_OVERRIDES.get(f"{cands[0]['path']}::{name}") or [[s["start"], s["end"]] for s in cands]
                rel.append({"path": cands[0]["path"], "grade": grade, "spans": spans, "label": f"sym:{name}"})
            elif kind == "file":
                if ref not in lines:
                    errors.append(f"{q['qid']}: unknown file {ref}")
                    continue
                rel.append({"path": ref, "grade": grade, "spans": [[1, lines[ref]]], "label": "file"})
            elif kind == "span":
                m = re.fullmatch(r"(.+):(\d+)-(\d+)", ref)
                if not m or m.group(1) not in lines:
                    errors.append(f"{q['qid']}: bad span {ref}")
                    continue
                a, b = int(m.group(2)), int(m.group(3))
                if not (1 <= a <= b <= lines[m.group(1)]):
                    errors.append(f"{q['qid']}: span out of range {ref} (file has {lines[m.group(1)]} lines)")
                    continue
                rel.append({"path": m.group(1), "grade": grade, "spans": [[a, b]], "label": "span"})
            else:
                errors.append(f"{q['qid']}: unknown label kind {kind}")
        if q.get("refs"):
            ident = q["query"]
            labelled = {r["path"] for r in rel}
            pat = re.compile(r"(?<![\w$])" + re.escape(ident) + r"(?![\w$])")
            hits = []
            for p in lines:
                if p in labelled:
                    continue
                occ = [i + 1 for i, l in enumerate(text(p)) if pat.search(l)]
                if occ:
                    hits.append((len(occ), p, occ))
            hits.sort(key=lambda h: (-h[0], h[1]))
            for _, p, occ in hits[:REF_CAP]:
                spans = []
                for o in occ:
                    a, b = max(1, o - REF_CTX), min(lines[p], o + REF_CTX)
                    if spans and a <= spans[-1][1] + 1:
                        spans[-1][1] = max(spans[-1][1], b)
                    else:
                        spans.append([a, b])
                rel.append({"path": p, "grade": 1, "spans": spans, "label": "ref"})
        if not any(r["grade"] >= 2 for r in rel):
            errors.append(f"{q['qid']}: no relevant (grade>=2) label")
        out.append({"qid": q["qid"], "query": q["query"], "kind": q["kind"], "relevant": rel})
    if errors:
        print("\n".join(errors), file=sys.stderr)
        sys.exit(1)
    with open(OUT, "w", encoding="utf-8", newline="\n") as fh:
        for q in out:
            fh.write(json.dumps(q, ensure_ascii=False) + "\n")
    kinds = defaultdict(int)
    for q in out:
        kinds[q["kind"]] += 1
    n_units = sum(len(q["relevant"]) for q in out)
    print(f"{len(out)} queries {dict(kinds)}; {n_units} labelled units -> {OUT}")


if __name__ == "__main__":
    main()
