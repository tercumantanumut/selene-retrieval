#!/usr/bin/env bash
# Reproduce the Selene V2 baseline result files under eval/baselines/.
#   eval/v2-baseline/run_baseline.sh <corpus> [variant-tag ...]
# corpus: selene-src | cqadupstack-webmasters; variant tags: large-fp32 small-fp32 small-q8
# Needs: node (Selene's node_modules for gpt-tokenizer), eval/.cache/venv (onnxruntime-directml,
# numpy, tokenizers), models in eval/.cache/models. Set EMBED_PROVIDER=cpu to avoid DirectML.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
eval_dir="$(dirname "$here")"
if command -v cygpath > /dev/null; then here="$(cygpath -m "$here")"; eval_dir="$(cygpath -m "$eval_dir")"; fi
corpus="$1"; shift
variants=("${@:-large-fp32 small-fp32 small-q8}")
read -r -a variants <<< "${variants[*]}"
py="$eval_dir/.cache/venv/Scripts/python.exe"; [ -f "$py" ] || py="$eval_dir/.cache/venv/bin/python"
work="$eval_dir/.cache/v2/$corpus"; mkdir -p "$work" "$eval_dir/baselines"
provider="${EMBED_PROVIDER:-dml}"

if [ ! -f "$work/chunks.jsonl" ]; then
  if [ "$corpus" = selene-src ]; then
    node "$here/v2_prep.mjs" corpus "$eval_dir/corpora/selene-src/manifest.json" "$eval_dir/.cache/corpora/selene-src" "$work/chunks.jsonl"
  else
    node "$here/v2_prep.mjs" corpus "$eval_dir/.cache/$corpus/corpus.jsonl" - "$work/chunks.jsonl" --raw-text
  fi
fi
node "$here/v2_prep.mjs" queries "$eval_dir/golden/$corpus.jsonl" "$work/queries.prepped.jsonl"

for tag in "${variants[@]}"; do
  case "$tag" in
    large-fp32) model=Xenova__bge-large-en-v1.5; onnx=model.onnx; label="bge-large-en-v1.5 fp32 (Selene local default)";;
    small-fp32) model=Xenova__bge-small-en-v1.5; onnx=model.onnx; label="bge-small-en-v1.5 fp32";;
    small-q8)   model=Xenova__bge-small-en-v1.5; onnx=model_quantized.onnx; label="bge-small-en-v1.5 q8";;
    *) echo "unknown variant $tag" >&2; exit 2;;
  esac
  [ -f "$work/emb-$tag.npy" ] || "$py" "$here/v2_embed.py" "$eval_dir/.cache/models/$model" "$work/chunks.jsonl" "$work/emb-$tag.npy" --onnx "$onnx" --provider "$provider" --batch 64
  "$py" "$here/v2_embed.py" "$eval_dir/.cache/models/$model" "$work/queries.prepped.jsonl" "$work/qemb-$tag.npy" --onnx "$onnx" --provider "$provider" > /dev/null
  # Committed baseline: lexical zero-fill "none" (conservative). The as-shipped LanceDB padding
  # model ("rowid") is written to the cache and diffed; see docs/research/m1-f-eval.md.
  out="$eval_dir/baselines/$corpus.selene-v2.$tag.json"
  "$py" "$here/v2_search.py" "$work/chunks.jsonl" "$work/emb-$tag.npy" "$work/queries.prepped.jsonl" "$work/qemb-$tag.npy" \
    "$out" --variant "$label" --corpus "$corpus" --zero-fill none --dump-candidates "$work/candidates-$tag.jsonl"
  "$py" "$here/v2_search.py" "$work/chunks.jsonl" "$work/emb-$tag.npy" "$work/queries.prepped.jsonl" "$work/qemb-$tag.npy" \
    "$work/results-$tag.zf-rowid.json" --variant "$label; zero-fill=rowid" --corpus "$corpus" --zero-fill rowid
  "$py" - "$out" "$work/results-$tag.zf-rowid.json" <<'PY'
import json, sys
a, b = (json.load(open(p, encoding="utf-8"))["results"] for p in sys.argv[1:3])
key = lambda r: [(h["path"], h["start_line"], h["end_line"]) for h in r["hits"]]
d = sum(1 for x, y in zip(a, b) if key(x) != key(y))
print(f"zero-fill rowid vs none: {d} of {len(a)} queries rank differently")
PY
  node --disable-warning=ExperimentalWarning "$here/verify_port.mjs" "$corpus" "$work/candidates-$tag.jsonl" 2> /dev/null | tail -1
  if [ "$tag" = large-fp32 ]; then  # arm ablations (not Selene configurations)
    mkdir -p "$eval_dir/baselines/ablations"
    for mode in dense lexical; do
      "$py" "$here/v2_search.py" "$work/chunks.jsonl" "$work/emb-$tag.npy" "$work/queries.prepped.jsonl" "$work/qemb-$tag.npy" \
        "$eval_dir/baselines/ablations/$corpus.selene-v2-$mode-only.$tag.json" --variant "ablation: $mode arm only ($label)" \
        --corpus "$corpus" --mode "$mode"
    done
  fi
done
