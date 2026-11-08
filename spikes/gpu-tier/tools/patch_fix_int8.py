"""Second doc patch: int8 is CPU-pinned (measured), Minimum bundles int8 + fp16, ORT implicit fallback finding."""
import pathlib

D = pathlib.Path(__file__).resolve().parents[3] / "docs"


def patch(name, pairs):
    p = D / name
    s = p.read_text(encoding="utf-8")
    for a, b in pairs:
        assert a in s, f"{name}: {a[:70]}"
        s = s.replace(a, b, 1)
    p.write_text(s, encoding="utf-8")
    print("patched", name)


patch("00-review-and-decisions.md", [
    ("| **Minimum** (\"Basic\", built in) | BAAI/bge-small-en-v1.5 (384), int8 ONNX, 32 MB | MIT | Always. This is the offline zero-config fallback. | **Bundled**; sha256 checked at load |",
     "| **Minimum** (\"Basic\", built in) | BAAI/bge-small-en-v1.5 (384): int8 for CPU (32 MB) + fp16 for GPU (64 MB) | MIT | Always. This is the offline zero-config fallback. | **Bundled** (both files, 96 MB); sha256 checked at load |"),
    ("- **Sticky variant.** An index keeps the exact model file it was built with. A later EP change (GPU removed or added) never mixes embedding spaces and never forces a re-embed.",
     "- **Sticky variant.** An index keeps the exact model file it was built with: the fp16 file if the build started on the GPU, the int8 file if on the CPU.\n"
     "  - **fp16/fp32 files are EP-portable.** CPU vs DirectML cosine was ≥ 0.99998 on four models. An fp16 index keeps working on the CPU if the GPU disappears.\n"
     "  - **int8 files are CPU-pinned.** DirectML int8 vs CPU int8 measured cosine 0.991–0.997, below the 0.999 bar, so an int8 index is always embedded and queried on the CPU.\n"
     "  - Either way, an EP change never mixes embedding spaces and never forces a re-embed.\n"
     "  - *Bundling the fp16 Minimum file (+64 MB) is what lets GPU machines on Minimum use the GPU with no download. The alternative is a CPU-only Minimum; that's the user's call if installer size matters more.*"),
    ("- **Open gate (M1):** the bundled int8 variant drifts from fp32 (top-10 neighbour overlap 0.855).",
     "- **Open gate (M1):** the bundled int8 variant (used on CPU-only machines) drifts from fp32 (top-10 neighbour overlap 0.855)."),
    ("**Silent CPU fallback ladder.** Failures never reach the user as errors:",
     "**Silent CPU fallback ladder.** Failures never reach the user as errors. *Never rely on ORT's implicit fallback*: measured with an fp16 model, a DirectML registration failure without `error_on_failure` left a CPU session that failed to initialise (graph-optimizer cast error). Always register with `error_on_failure`, then build a fresh CPU session:"),
])

patch("02-architecture.md", [
    ("**Sticky variant.** An index is built with exactly one model file, `gpu_file` or `cpu_file` depending on the EP *when the build starts*, and records its sha256. Later incremental embeds and every query use that same file on whatever EP is current. ORT runs fp16 graphs on the CPU and int8 graphs on DirectML: slower, but correct. Embedding spaces are never mixed, and an EP change never forces a re-embed.",
     "**Sticky variant.** An index is built with exactly one model file, `gpu_file` (fp16) or `cpu_file` (int8) depending on the EP *when the build starts*, and records its sha256. Later incremental embeds and every query use that same file.\n"
     "- `ep_for_file`: **fp16/fp32 → current EP (portable).** CPU vs DirectML cosine ≥ 0.99998 for bge-small, arctic-m, arctic-l and gte-large. If the GPU goes away, the same fp16 file runs on the CPU.\n"
     "- **int8 → always CPU.** DirectML int8 vs CPU int8 gives cosine 0.991–0.997, below the 0.999 bar. If a GPU appears later, the int8 index stays on the CPU, and a query embed on the CPU costs only milliseconds.\n"
     "- Embedding spaces are never mixed, and an EP change never forces a re-embed.\n"
     "- Never rely on ORT's implicit EP fallback. With an fp16 model it produced a session that failed to initialise. Always register with `error_on_failure` and build the CPU session explicitly."),
])

patch("01-research-summary.md", [
    ("| Embedding tiers (Amendment 1) | **Minimum** bge-small-en-v1.5 int8 (384, MIT, 32 MB, bundled)",
     "| Embedding tiers (Amendment 1) | **Minimum** bge-small-en-v1.5 (384, MIT; int8 32 MB for CPU + fp16 64 MB for GPU, both bundled; int8 is CPU-pinned)"),
])
