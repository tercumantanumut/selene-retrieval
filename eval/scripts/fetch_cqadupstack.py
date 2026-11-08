"""Fetch the public `cqadupstack-webmasters` corpus (BEIR / MTEB copy) at a pinned revision,
verify sha256, and (re)generate the golden file eval/golden/cqadupstack-webmasters.jsonl.

Licence: BEIR distributes CQADupStack under Apache-2.0; the text is StackExchange user content
(CC BY-SA). We keep only queries + qrels (derived labels) in the repo with attribution; the corpus
itself is downloaded into eval/.cache/ and never committed. See eval/corpora/cqadupstack-webmasters/.

Usage: python eval/scripts/fetch_cqadupstack.py [--golden-only]
"""
import argparse, hashlib, json, os, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
EVAL = os.path.dirname(HERE)
REPO = "mteb/cqadupstack-webmasters"
REVISION = "c691d4af16c19a79df7ee93bfbd12cc186a5bc92"
FILES = ["corpus.jsonl", "queries.jsonl", "qrels/test.tsv"]
CACHE = os.path.join(EVAL, ".cache", "cqadupstack-webmasters")
MANIFEST = os.path.join(EVAL, "corpora", "cqadupstack-webmasters", "manifest.json")


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for block in iter(lambda: fh.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def fetch():
    os.makedirs(os.path.join(CACHE, "qrels"), exist_ok=True)
    pinned = json.load(open(MANIFEST, encoding="utf-8"))["files"] if os.path.exists(MANIFEST) else {}
    out = {}
    for f in FILES:
        dest = os.path.join(CACHE, f)
        if not os.path.exists(dest):
            url = f"https://huggingface.co/datasets/{REPO}/resolve/{REVISION}/{f}"
            print("downloading", url)
            urllib.request.urlretrieve(url, dest)
        digest = sha256(dest)
        if f in pinned and pinned[f]["sha256"] != digest:
            raise SystemExit(f"sha256 mismatch for {f}: {digest} != {pinned[f]['sha256']}")
        out[f] = {"sha256": digest, "bytes": os.path.getsize(dest)}
    return out


def golden():
    queries = {}
    with open(os.path.join(CACHE, "queries.jsonl"), encoding="utf-8") as fh:
        for line in fh:
            q = json.loads(line)
            queries[q["_id"]] = q["text"]
    rel = {}
    with open(os.path.join(CACHE, "qrels", "test.tsv"), encoding="utf-8") as fh:
        next(fh)
        for line in fh:
            qid, did, score = line.rstrip("\n").split("\t")
            if int(score) > 0:
                # binary qrels -> grade 3 ("answers the query"); nDCG is scale-invariant for binary
                # labels, and recall/MRR count grade >= 2 (eval/golden/README.md)
                rel.setdefault(qid, []).append({"path": did, "grade": 3})
    path = os.path.join(EVAL, "golden", "cqadupstack-webmasters.jsonl")
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8", newline="\n") as fh:
        for qid in sorted(rel, key=lambda x: int(x)):
            fh.write(json.dumps({"qid": f"W{qid}", "query": queries[qid], "kind": "nl",
                                 "relevant": sorted(rel[qid], key=lambda r: r["path"])}) + "\n")
    print(f"{len(rel)} queries -> {path}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--golden-only", action="store_true")
    a = ap.parse_args()
    if not a.golden_only:
        files = fetch()
        os.makedirs(os.path.dirname(MANIFEST), exist_ok=True)
        with open(MANIFEST, "w", encoding="utf-8", newline="\n") as fh:
            json.dump({
                "corpus": "cqadupstack-webmasters",
                "source": f"https://huggingface.co/datasets/{REPO}",
                "revision": REVISION,
                "licence": "Apache-2.0 (BEIR/MTEB packaging); text is StackExchange content, CC BY-SA",
                "doc_text": "title + \"\\n\\n\" + text, ingested via Selene's raw-text path",
                "files": files,
            }, fh, indent=1)
            fh.write("\n")
    golden()


if __name__ == "__main__":
    main()
