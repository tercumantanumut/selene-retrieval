"""Reference engine for harness sanity checks: textbook BGE usage on a BEIR-style corpus.

CLS pooling + L2 normalise, the official BGE query instruction ("Represent this sentence for
searching relevant passages: "), documents embedded whole (title + text, truncated at 512 tokens),
exact cosine top-k. This is what MTEB reports for BGE, so on CQADupStack-webmasters it should land
near the published nDCG@10. That validates the golden ids, qrels and metric plumbing end to end.
It is NOT a Selene configuration.

  python reference_dense.py <model_dir> <corpus.jsonl> <golden.jsonl> <out.json> [--onnx model.onnx]
        [--provider dml|cpu] [--k 50]
"""
import argparse, json, os, sys, time

import numpy as np
import onnxruntime as ort
from tokenizers import Tokenizer

QUERY_INSTRUCTION = "Represent this sentence for searching relevant passages: "


def session(model_dir, onnx, provider):
    so = ort.SessionOptions()
    providers = ["CPUExecutionProvider"]
    if provider == "dml":
        so.enable_mem_pattern = False
        so.execution_mode = ort.ExecutionMode.ORT_SEQUENTIAL
        providers = ["DmlExecutionProvider", "CPUExecutionProvider"]
    return ort.InferenceSession(os.path.join(model_dir, "onnx", onnx), so, providers=providers)


def embed(sess, tok, texts, batch=64):
    names = {i.name for i in sess.get_inputs()}
    lengths = [len(e.ids) for e in tok.encode_batch(texts)]
    order = np.argsort(lengths, kind="stable")
    out = None
    for s in range(0, len(texts), batch):
        idx = order[s:s + batch]
        enc = tok.encode_batch([texts[i] for i in idx])
        ids = np.array([e.ids for e in enc], dtype=np.int64)
        mask = np.array([e.attention_mask for e in enc], dtype=np.int64)
        feeds = {"input_ids": ids, "attention_mask": mask}
        if "token_type_ids" in names:
            feeds["token_type_ids"] = np.zeros_like(ids)
        cls = sess.run(None, feeds)[0][:, 0, :]
        cls = cls / np.clip(np.linalg.norm(cls, axis=1, keepdims=True), 1e-12, None)
        if out is None:
            out = np.zeros((len(texts), cls.shape[1]), dtype=np.float32)
        out[idx] = cls
    return out


def main():
    ap = argparse.ArgumentParser()
    for a in ("model_dir", "corpus", "golden", "out"):
        ap.add_argument(a)
    ap.add_argument("--onnx", default="model.onnx")
    ap.add_argument("--provider", choices=["dml", "cpu"], default="dml")
    ap.add_argument("--k", type=int, default=50)
    a = ap.parse_args()
    tok = Tokenizer.from_file(os.path.join(a.model_dir, "tokenizer.json"))
    tok.enable_truncation(512)
    tok.enable_padding(pad_id=0, pad_token="[PAD]")
    sess = session(a.model_dir, a.onnx, a.provider)
    ids, texts = [], []
    with open(a.corpus, encoding="utf-8") as fh:
        for line in fh:
            d = json.loads(line)
            ids.append(d["_id"])
            texts.append(f"{d.get('title', '')}\n\n{d['text']}".strip())
    with open(a.golden, encoding="utf-8") as fh:
        golden = [json.loads(l) for l in fh if l.strip()]
    t0 = time.time()
    D = embed(sess, tok, texts)
    Q = embed(sess, tok, [QUERY_INSTRUCTION + q["query"] for q in golden])
    print(f"embedded {len(texts)} docs + {len(golden)} queries in {time.time() - t0:.0f}s", file=sys.stderr)
    results = []
    for q, qv in zip(golden, Q):
        sims = D @ qv
        top = np.argpartition(-sims, a.k)[:a.k]
        top = top[np.argsort(-sims[top], kind="stable")]
        results.append({"qid": q["qid"], "latency_ms": None,
                        "hits": [{"path": ids[i], "score": round(float(sims[i]), 6)} for i in top]})
    head = {"engine": "reference-dense",
            "variant": f"{os.path.basename(a.model_dir)} CLS + BGE query instruction, whole docs"}
    with open(a.out, "w", encoding="utf-8", newline="\n") as fh:  # compact, one result per line
        fh.write(json.dumps(head, separators=(",", ":"))[:-1] + ',"results":[\n')
        fh.write(",\n".join(json.dumps(r, separators=(",", ":")) for r in results))
        fh.write("\n]}\n")
    print(f"{len(results)} queries -> {a.out}")


if __name__ == "__main__":
    main()
