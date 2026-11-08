#!/usr/bin/env bash
# Runs spike tests against a staged exe dir. Usage: ./run-tests.sh <stage_dir> <out_dir> [tests...]
# tests: modules fts5 e2e embed tgc l1051 (default: all)
HERE="$(cd "$(dirname "$0")" && pwd)"
STAGE="$1"; OUT="$2"; shift 2
T="${*:-modules fts5 embed e2e tgc l1051}"
mkdir -p "$OUT"
EXE="$STAGE/native-stack-spike.exe"
# stripped environment: System32 + the decoy dir only, no cargo/python/VS on PATH
SP="/c/Windows/System32:/c/Windows:$HERE/target/decoy-path"
run() { name=$1; shift; echo "== $name: $*" | tee "$OUT/$name.log"; ( cd "$HERE" && env -i SYSTEMROOT='C:\Windows' WINDIR='C:\Windows' TEMP="$TEMP" TMP="$TMP" USERPROFILE="$USERPROFILE" LOCALAPPDATA="$LOCALAPPDATA" APPDATA="$APPDATA" PATH="$SP" SPIKE_LIST_MODULES=1 "$EXE" "$@" ) >> "$OUT/$name.log" 2>&1; echo "exit=$?" >> "$OUT/$name.log"; tail -1 "$OUT/$name.log"; }
for t in $T; do case $t in
  modules) run modules modules ;;
  fts5) run fts5 fts5 ;;
  embed) for v in "bge-small-en-v1.5 model.onnx" "bge-small-en-v1.5 model_int8.onnx" "bge-small-en-v1.5 model_quantized.onnx" "bge-small-en-v1.5-q model_optimized.onnx"; do set -- $v; n="${2%.onnx}"; run "embed-$1-$n" embed "models/$1" "$2" test11/sentences.json "test11/rust_$1_$n.json"; done ;;
  e2e) run e2e e2e models/bge-small-en-v1.5-q model_optimized.onnx 2000 ;;
  tgc) run tgc-hold tantivy-gc target/tgc-hold 300 1; run tgc-nohold tantivy-gc target/tgc-nohold 300 0 ;;
  l1051) run l1051 lance1051 target/l1051 ;;
esac; done
