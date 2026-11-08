#!/usr/bin/env bash
# M5-E ranking evals (R-UX37 exact-lane gate, R-UX36 reranker). Release bench only; reuses an existing index.
#
#   bash eval/scripts/m5e_eval.sh <part> <out-dir> [extra bench args for every engine of the part]
#     part: src | exact | neg | cqa | rerank (variants as extra args: "<label>=<bench flags>" ...)
#
# Environment (defaults match the M5-E layout; the M4 tester's indexes were copied there):
#   BENCH   sr-bench-engine.exe (release) with sr-extract.exe next to it
#   XTASK   xtask.exe (any build; the eval runner has no engine code)
#   SRC_IDX / CQA_IDX  index dirs built by the bench (`--rebuild` once)
#   IDLE    idle-wait PowerShell script (optional)
set -u
PART=${1:?part}
OUT=${2:?out dir}
shift 2
R=$(cd "$(dirname "$0")/../.." && pwd); W=$(cygpath -w "$R")
T=$R/target/m5-e
BENCH=${BENCH:-$T/ev/bin/sr-bench-engine.exe}
XTASK=${XTASK:-$R/target/m4-test/debug/xtask.exe}
SRC_IDX=${SRC_IDX:-$(cygpath -w "$T/ev/s")}
CQA_IDX=${CQA_IDX:-$(cygpath -w "$T/ev/cqa")}
IDLE=${IDLE:-$T/waitidle.ps1}
APP="$W\\crates\\providers"
SRC="$W\\eval\\.cache\\corpora\\selene-src"
CQ="$W\\eval\\.cache\\cqadupstack-webmasters\\corpus.jsonl"
BE=$(cygpath -w "$BENCH")
mkdir -p "$OUT"; O=$(cygpath -w "$OUT"); S=$OUT/summary.txt
idle() { [ -f "$IDLE" ] && echo "[$1] $(powershell.exe -NoProfile -ExecutionPolicy Bypass -File "$(cygpath -w "$IDLE")" -Max 35 -TimeoutSec 300)" >> "$S"; }
SB="--index $SRC_IDX --corpus-dir $SRC --app-dir $APP --ep cpu"
CB="--index $CQA_IDX --corpus-jsonl $CQ --app-dir $APP --ep cpu"
ev() { # ev <name> <golden> <title> <engine specs...>
  local name=$1 golden=$2 title=$3; shift 3
  local args=()
  for e in "$@"; do args+=(--engine "$e"); done
  "$XTASK" eval --golden "$golden" "${args[@]}" --min-ndcg-delta -100 --timeout-secs 3600 \
    --title "$title" --out "$O\\$name.md" --json "$O\\$name.json" > "$OUT/$name.out" 2>&1
  echo "[$name] rc=$?" >> "$S"
}
case $PART in
  src)
    idle src
    ev src eval/golden/selene-src.jsonl "M5-E: selene-src, exact lane gated vs ungated (M4) vs off" \
      "v2=results:eval/baselines/selene-src.selene-v2.large-fp32.json" \
      "gated=cmd:$BE $SB --exact-lane on" \
      "ungated=cmd:$BE $SB --exact-lane ungated" \
      "off=cmd:$BE $SB --exact-lane off" ;;
  exact)
    idle exact
    ev exact eval/golden/selene-src.exact.jsonl "M5-E: R-UX33 exact identifiers / numbers (23)" \
      "gated=cmd:$BE $SB --exact-lane on --cutoff on" \
      "ungated=cmd:$BE $SB --exact-lane ungated --cutoff on" ;;
  neg)
    for lane in on ungated; do
      for g in exact exact-negatives negatives; do
        node "$(cygpath -w "$T/neg.mjs")" "$O\\neg-$g-$lane.txt" "$W\\eval\\golden\\selene-src.$g.jsonl" "$BE" \
          --index "$SRC_IDX" --corpus-dir "$SRC" --app-dir "$APP" --ep cpu --exact-lane $lane --cutoff on >> "$S" 2>&1
        echo "[neg $g lane=$lane] rc=$?" >> "$S"
      done
    done ;;
  cqa)
    idle cqa
    ev cqa eval/golden/cqadupstack-webmasters.jsonl "M5-E: CQADupStack-webmasters, exact lane gated vs ungated vs off" \
      "v2=results:eval/baselines/cqadupstack-webmasters.selene-v2.large-fp32.json" \
      "gated=cmd:$BE $CB --exact-lane on" \
      "ungated=cmd:$BE $CB --exact-lane ungated" \
      "off=cmd:$BE $CB --exact-lane off" ;;
  rerank)
    # RR_IDX: a copy of the selene-src index (turning the reranker on persists in its settings.toml).
    # RR_CORPUS: "--corpus-jsonl <file>" for CQADupStack (default: the selene-src corpus dir).
    RR=${RR_IDX:?RR_IDX}; RB="--index $RR ${RR_CORPUS:---corpus-dir $SRC} --app-dir $APP --ep cpu --rerank on"
    idle rerank
    # RR_GOLDEN / RR_BASE (the non-rerank engine flags) let the same part run on CQADupStack.
    specs=("off=cmd:$BE ${RR_BASE:-$SB}")
    for v in "$@"; do specs+=("${v%%=*}=cmd:$BE $RB ${v#*=}"); done
    ev "rerank${RR_TAG:-}" "${RR_GOLDEN:-eval/golden/selene-src.jsonl}" \
      "M5-E: reranker variants (ms-marco-MiniLM-L-6-v2, CPU)" "${specs[@]}" ;;
  *) echo "unknown part $PART"; exit 2 ;;
esac
echo "DONE $PART" >> "$S"
