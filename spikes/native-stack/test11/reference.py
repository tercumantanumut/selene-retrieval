"""Test 11 reference: sentence-transformers BAAI/bge-small-en-v1.5 (CLS pooling + L2 normalise)
vs Rust fastembed outputs. Usage: reference.py ref|compare <file...>"""
import json, sys
import numpy as np
here = __file__.rsplit("\\", 1)[0] if "\\" in __file__ else __file__.rsplit("/", 1)[0]
texts = json.load(open(f"{here}/sentences.json", encoding="utf-8"))
if sys.argv[1] == "ref":
    from sentence_transformers import SentenceTransformer
    import sentence_transformers, torch
    m = SentenceTransformer("BAAI/bge-small-en-v1.5", device="cpu")
    print("st", sentence_transformers.__version__, "torch", torch.__version__, "pooling:", m[1].get_config_dict(), "max_seq:", m.max_seq_length)
    v = m.encode(texts, normalize_embeddings=True, batch_size=16)
    json.dump(v.tolist(), open(f"{here}/ref_st.json", "w"))
    print("wrote", v.shape)
else:
    ref = np.array(json.load(open(f"{here}/ref_st.json")))
    for f in sys.argv[2:]:
        o = np.array(json.load(open(f)))
        o = o / np.linalg.norm(o, axis=1, keepdims=True)
        cos = (ref * o).sum(1)
        print(f"{f.rsplit('/',1)[-1]:<26} n={len(cos)} min={cos.min():.6f} mean={cos.mean():.6f} "
              f"argmin={int(cos.argmin())} pass(>=0.999)={bool(cos.min() >= 0.999)}")
