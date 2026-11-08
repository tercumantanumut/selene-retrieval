"""Build the R-UX33 exact-identifier / number golden sets (M3-Q) from the pinned selene-src corpus.

  eval/golden/selene-src.exact.jsonl            positives: numbers, identifiers with digits, compounds,
                                                and short phrases that contain one; every unit is each
                                                whole-word occurrence line +-2 (grade 3)
  eval/golden/selene-src.exact-negatives.jsonl  numbers / identifiers that occur nowhere in the corpus
                                                (checked here, case-insensitive substring), expected
                                                answer: "Nothing closely matched"

Fails loudly when a positive has no occurrence or a negative occurs. Needs the materialised corpus
(python eval/scripts/build_selene_corpus.py). Usage: python eval/scripts/build_exact_golden.py
"""
import json, os, re, sys

EVAL = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ROOT = os.path.join(EVAL, ".cache", "corpora", "selene-src")
OUT_POS = os.path.join(EVAL, "golden", "selene-src.exact.jsonl")
OUT_NEG = os.path.join(EVAL, "golden", "selene-src.exact-negatives.jsonl")
CTX = 2

# (qid, query, kind, token that must occur as a whole word)
POSITIVES = [
    ("E001", "135000", "number", "135000"),
    ("E002", "15000", "number", "15000"),
    ("E003", "45000", "number", "45000"),
    ("E004", "372000", "number", "372000"),
    ("E005", "20250219", "number", "20250219"),
    ("E006", "140000", "number", "140000"),
    ("E007", "36742", "number", "36742"),
    ("E008", "65536", "number", "65536"),
    ("E009", "1071006060591", "number", "1071006060591"),
    ("E010", "276_000", "number", "276_000"),
    ("E011", "887a0005", "exact_identifier", "887a0005"),
    ("E012", "o200k_base", "exact_identifier", "o200k_base"),
    ("E013", "isGpt54", "exact_identifier", "isGpt54"),
    ("E014", "ensureV2SchemaCompatibility", "exact_identifier", "ensureV2SchemaCompatibility"),
    ("E015", "decodeBase64Url", "exact_identifier", "decodeBase64Url"),
    ("E016", "gpt_5_2_prompt", "exact_identifier", "gpt_5_2_prompt"),
    ("E017", "Wan22VideoResult", "exact_identifier", "Wan22VideoResult"),
    ("E018", "callWan22Video", "exact_identifier", "callWan22Video"),
    ("E019", "claude-3-7-sonnet-20250219", "exact_identifier", "claude-3-7-sonnet-20250219"),
    ("E020", "which timeout is set to 135000", "number_phrase", "135000"),
    ("E021", "oauth callback port 36742", "number_phrase", "36742"),
    ("E022", "887A0005 device removed error", "number_phrase", "887a0005"),
    ("E023", "token threshold 140000 for compaction", "number_phrase", "140000"),
]

NEGATIVES = [
    ("XE01", "48213907", "48213907"),
    ("XE02", "20991231", "20991231"),
    ("XE03", "90417", "90417"),
    ("XE04", "73591", "73591"),
    ("XE05", "887a0099", "887a0099"),
    ("XE06", "0x80070057", "0x80070057"),
    ("XE07", "isGpt99", "isGpt99"),
    ("XE08", "Wan99VideoResult", "Wan99VideoResult"),
    ("XE09", "o999k_base", "o999k_base"),
    ("XE10", "callWan77Video", "callWan77Video"),
    ("XE11", "MAX_RETRY_7731_MS", "MAX_RETRY_7731_MS"),
    ("XE12", "v9.81.4", "v9.81.4"),
    ("XE13", "decodeBase91Url", "decodeBase91Url"),
    ("XE14", "gpt_9_9_prompt", "gpt_9_9_prompt"),
    ("XE15", "ensureV7SchemaCompatibility", "ensureV7SchemaCompatibility"),
    ("XE16", "invoice number 48213907 from the supplier", "48213907"),
]


def load():
    files = []
    for dp, _, fn in os.walk(ROOT):
        for f in sorted(fn):
            p = os.path.join(dp, f)
            try:
                t = open(p, encoding="utf-8").read().replace("\r\n", "\n").split("\n")
            except (UnicodeDecodeError, OSError):
                continue
            files.append((os.path.relpath(p, ROOT).replace(os.sep, "/"), t))
    return sorted(files)


def main():
    if not os.path.isdir(ROOT):
        sys.exit(f"corpus not materialised: {ROOT}")
    files = load()
    errors, pos, neg = [], [], []
    for qid, query, kind, tok in POSITIVES:
        pat = re.compile(r"(?<![A-Za-z0-9_])" + re.escape(tok) + r"(?![A-Za-z0-9_])", re.I)
        rel = []
        for path, lines in files:
            hits = [i for i, l in enumerate(lines, 1) if pat.search(l)]
            if not hits:
                continue
            spans = []
            for i in hits:
                a, b = max(1, i - CTX), min(len(lines), i + CTX)
                if spans and a <= spans[-1][1] + 1:
                    spans[-1][1] = max(spans[-1][1], b)
                else:
                    spans.append([a, b])
            rel.append({"path": path, "grade": 3, "spans": spans, "label": f"exact:{tok}"})
        if not rel:
            errors.append(f"{qid}: {tok!r} occurs nowhere")
        pos.append({"qid": qid, "query": query, "kind": kind, "relevant": rel})
    for qid, query, tok in NEGATIVES:
        pat = re.compile(re.escape(tok), re.I)
        if any(pat.search(l) for _, lines in files for l in lines):
            errors.append(f"{qid}: negative {tok!r} occurs in the corpus")
        neg.append({"qid": qid, "query": query, "kind": "negative", "relevant": []})
    if errors:
        sys.exit("\n".join(errors))
    with open(OUT_POS, "w", encoding="utf-8", newline="\n") as f:
        for q in pos:
            f.write(json.dumps(q) + "\n")
    with open(OUT_NEG, "w", encoding="utf-8", newline="\n") as f:
        for q in neg:
            f.write(json.dumps(q) + "\n")
    print(f"{len(pos)} positives, {len(neg)} negatives")


if __name__ == "__main__":
    main()
