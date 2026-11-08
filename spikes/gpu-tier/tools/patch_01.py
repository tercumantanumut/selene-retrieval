"""One-off doc patch for docs/01-research-summary.md (M0-E rows)."""
import pathlib, sys

p = pathlib.Path(sys.argv[1])
s = p.read_text(encoding="utf-8")
assert "Embedding tiers" not in s, "already patched"


def rep(a, b):
    global s
    assert a in s, a[:70]
    s = s.replace(a, b, 1)


old_embed = next(l for l in s.splitlines() if l.startswith("| Local embeddings / rerank |"))
rep(old_embed,
    "| Local embeddings / rerank | **fastembed =7.1.1**, `default-features=false`, `ort-download-binaries-rustls-tls` + **`directml`** "
    "(ort =2.0.0-rc.13). ORT static where it links, else Microsoft `onnxruntime.dll` 1.24.x via `load-dynamic` by absolute path "
    "(M0-E linked and ran this way; static pyke build needs MSVC ≥ 14.42) | ort direct, candle 0.11 | Correct pooling and prefixes "
    "per model (W16); batched rerankers; `with_execution_providers` takes the DirectML EP. arctic-embed-m **v1.5** is not a fastembed "
    "built-in (its `SnowflakeArcticEmbedM` is v1), so it runs as a `UserDefinedEmbeddingModel` (CLS pooling, sha-pinned files) | "
    "Minimum model **bundled**; Standard/High one click with consent, size shown, resume, sha256. DLLs app-local, signature-checked; "
    "the user installs nothing. |")
old_default = next(l for l in s.splitlines() if l.startswith("| Default profile |"))
rep(old_default,
    "| Embedding tiers (Amendment 1) | **Minimum** bge-small-en-v1.5 int8 (384, MIT, 32 MB, bundled) · **Standard** "
    "snowflake-arctic-embed-m-v1.5 (768, Apache-2.0; fp16 208 MB GPU / int8 105 MB CPU) · **High** snowflake-arctic-embed-l "
    "(1024, Apache-2.0; fp16 638 MB, GPU only). Multilingual-e5-small stays a separate one-click profile | bge-base (768, MTEB-R 53.25), "
    "nomic-v1.5 (768, 53.01; mean pooling, slower on GPU), bge-large / mxbai-large (1024, 54.29 / 54.39), gte-large-en-v1.5 (1024, "
    "**57.91** but 21 % slower and +0.6 GB VRAM; M1 eval may promote it for ≥ 8 GiB GPUs) | Highest BEIR-15 nDCG@10 per size class "
    "among verified ONNX builds: 51.68 / 55.14 / 55.98. One shared WordPiece vocab keeps chunk IDs tier-independent. Measured speeds: "
    "`research/m0-e-gpu-tier.md` | None for Minimum. Standard/High: one opt-in click on the \"Recommended for your computer\" line; "
    "download in background, search keeps working. |")
anchor = next(l for l in s.splitlines() if l.startswith("| Remote embeddings |"))
rep(anchor,
    "| GPU execution provider (Amendment 2) | **ORT DirectML EP** (DirectML.dll 1.15.x redistributable, app-local), on by default; "
    "iGPU via timed probe; silent CPU fallback ladder | Windows ML (post-v1: same ORT API, vendor EPs on 24H2+); CUDA/TensorRT EP "
    "(NVIDIA only, ~1 GB of DLLs); CPU only (v1 plan) | Any DX12 GPU on Win10 1903+; 18–50× CPU throughput measured; fp32/fp16 on "
    "DML match CPU fp32 (cos 1.00000). DirectML is in sustained engineering (R17) | **None**: no driver or runtime install beyond the "
    "GPU driver Windows already has; no setting. Advanced → \"Use graphics card: Off\" only. |\n"
    "| Hardware detection (Amendment 1) | `windows` 0.62: `GlobalMemoryStatusEx`, `GetLogicalProcessorInformationEx`, "
    "`GetDiskFreeSpaceExW`, DXGI `EnumAdapters1` + `D3D12CreateDevice` (FL 11.0) + `D3D12_FEATURE_ARCHITECTURE1.UMA`; "
    "`is_x86_feature_detected!(\"avx2\")` | sysinfo, WMI | No new crate (windows-sys/windows already used); ≈ 0.2 s; no admin; "
    "UMA flag stops APU carve-outs counting as dedicated VRAM | None: silent at first run; reasons only under Details. |\n"
    + anchor)
p.write_text(s, encoding="utf-8")
print("patched")
