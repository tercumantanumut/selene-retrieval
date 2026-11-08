"""Embed V2 chunk/query inputs exactly as Selene's local provider does.

Selene (lib/ai/local-embeddings.ts) runs Transformers.js `feature-extraction` over the Xenova ONNX
export with `{ pooling: "mean", normalize: true }`, tokenizer truncation at the model max (512), and
the "Represent this code for search:" prefix already applied by v2_prep.mjs (`embed_input`). This
script reproduces that with onnxruntime (CPU) + HF `tokenizers` using the same tokenizer.json and the
same ONNX file (Selene's default dtype is fp32 -> onnx/model.onnx; `--onnx model_quantized.onnx` is
Transformers.js "q8"). Mean pooling over the attention mask is padding-invariant, so inputs are
length-sorted for speed.

  python v2_embed.py <model_dir> <in.jsonl> <out.npy> [--field embed_input] [--onnx model.onnx]
  (queries.jsonl: every `expanded[*].embed_input` is embedded, in order)
"""
import argparse, json, os, sys, time

import numpy as np
import onnxruntime as ort
from tokenizers import Tokenizer


def load_inputs(path, field):
    texts = []
    with open(path, encoding="utf-8") as fh:
        for line in fh:
            if not line.strip():
                continue
            row = json.loads(line)
            if "expanded" in row:
                texts.extend(e[field] for e in row["expanded"])
            else:
                texts.append(row[field])
    return texts


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("model_dir")
    ap.add_argument("inp")
    ap.add_argument("out")
    ap.add_argument("--field", default="embed_input")
    ap.add_argument("--onnx", default="model.onnx")
    ap.add_argument("--batch", type=int, default=32)
    ap.add_argument("--threads", type=int, default=os.cpu_count())
    ap.add_argument("--provider", choices=["cpu", "dml"], default="cpu")
    a = ap.parse_args()

    tok = Tokenizer.from_file(os.path.join(a.model_dir, "tokenizer.json"))
    tok.enable_truncation(512)
    tok.enable_padding(pad_id=0, pad_token="[PAD]")
    so = ort.SessionOptions()
    so.intra_op_num_threads = a.threads
    so.graph_optimization_level = ort.GraphOptimizationLevel.ORT_ENABLE_ALL
    providers = ["CPUExecutionProvider"]
    if a.provider == "dml":  # DirectML requires sequential execution and no memory pattern
        so.enable_mem_pattern = False
        so.execution_mode = ort.ExecutionMode.ORT_SEQUENTIAL
        providers = ["DmlExecutionProvider", "CPUExecutionProvider"]
    sess = ort.InferenceSession(os.path.join(a.model_dir, "onnx", a.onnx), so, providers=providers)
    names = {i.name for i in sess.get_inputs()}

    texts = load_inputs(a.inp, a.field)
    lengths = [len(e.ids) for e in tok.encode_batch(texts)] if texts else []
    order = np.argsort(lengths, kind="stable")
    out = None
    t0 = time.time()
    for bstart in range(0, len(texts), a.batch):
        idx = order[bstart:bstart + a.batch]
        enc = tok.encode_batch([texts[i] for i in idx])
        ids = np.array([e.ids for e in enc], dtype=np.int64)
        mask = np.array([e.attention_mask for e in enc], dtype=np.int64)
        feeds = {"input_ids": ids, "attention_mask": mask}
        if "token_type_ids" in names:
            feeds["token_type_ids"] = np.zeros_like(ids)
        hidden = sess.run(None, feeds)[0]  # last_hidden_state [b, t, d]
        m = mask[..., None].astype(np.float32)
        pooled = (hidden * m).sum(1) / np.clip(m.sum(1), 1e-9, None)
        pooled /= np.clip(np.linalg.norm(pooled, axis=1, keepdims=True), 1e-12, None)
        if out is None:
            out = np.zeros((len(texts), pooled.shape[1]), dtype=np.float32)
        out[idx] = pooled.astype(np.float32)
        done = bstart + len(idx)
        if (bstart // a.batch) % 50 == 0:
            el = time.time() - t0
            print(f"{done}/{len(texts)} {el:.0f}s ({done / max(el, 1e-9):.1f}/s)", flush=True)
    np.save(a.out, out if out is not None else np.zeros((0, 0), dtype=np.float32))
    print(f"done {len(texts)} in {time.time() - t0:.1f}s -> {a.out}", file=sys.stderr)


if __name__ == "__main__":
    main()
