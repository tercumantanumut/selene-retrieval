"""Test 11 control: run each local ONNX variant with Python onnxruntime + HF tokenizer (CLS, L2) and
write ort_py_<name>.json, so quantisation error can be separated from fastembed pipeline error."""
import json, sys, numpy as np, onnxruntime as ort
from tokenizers import Tokenizer
here = __file__.replace("\\", "/").rsplit("/", 1)[0]
texts = json.load(open(f"{here}/sentences.json", encoding="utf-8"))
for d, f in [("bge-small-en-v1.5", "model.onnx"), ("bge-small-en-v1.5", "model_int8.onnx"),
             ("bge-small-en-v1.5", "model_quantized.onnx"), ("bge-small-en-v1.5-q", "model_optimized.onnx")]:
    md = f"{here}/../models/{d}"
    tok = Tokenizer.from_file(f"{md}/tokenizer.json")
    tok.enable_truncation(512); tok.enable_padding()
    s = ort.InferenceSession(f"{md}/{f}", providers=["CPUExecutionProvider"])
    names = [i.name for i in s.get_inputs()]
    out = []
    for i in range(0, len(texts), 16):
        enc = tok.encode_batch(texts[i:i+16])
        feed = {"input_ids": np.array([e.ids for e in enc], dtype=np.int64),
                "attention_mask": np.array([e.attention_mask for e in enc], dtype=np.int64),
                "token_type_ids": np.array([e.type_ids for e in enc], dtype=np.int64)}
        h = s.run(None, {k: v for k, v in feed.items() if k in names})[0]
        out.extend(h[:, 0, :].tolist())
    name = f"ort_py_{d}_{f}".replace(".onnx", "")
    json.dump(out, open(f"{here}/{name}.json", "w"))
    print("wrote", name, "inputs", names)
