#!/usr/bin/env bash
# Build the spike with the user-level MSVC 14.44 (VS 17.14) toolset downloaded by agent C (no admin).
# Usage: ./build-144.sh <target-subdir> [cargo args...]
set -e
HERE="$(cd "$(dirname "$0")" && pwd)"
R="$HERE/../extract-sandbox"
M="$R/vendor/msvc-dl/msvc/Contents/VC/Tools/MSVC/14.44.35207"
MW=$(cygpath -m "$M")
K="C:/Program Files (x86)/Windows Kits/10"
SDK=10.0.22621.0
export PATH="$HOME/.cargo/bin:$M/bin/Hostx64/x64:/c/protoc/bin:$PATH"
export VCINSTALLDIR="$(cygpath -w "$R/vendor/msvc-dl/msvc/Contents/VC")\\"
export VCToolsInstallDir="$(cygpath -w "$M")\\"
export VCToolsVersion=14.44.35207
export LIB="$MW/lib/x64;$K/Lib/$SDK/ucrt/x64;$K/Lib/$SDK/um/x64"
export INCLUDE="$MW/include;$K/Include/$SDK/ucrt;$K/Include/$SDK/um;$K/Include/$SDK/shared"
export PROTOC="C:/protoc/bin/protoc.exe"
sub="$1"; shift
export CARGO_TARGET_DIR="$(cygpath -w "$HERE/target/$sub")"
cd "$HERE"
which link cl
start=$(date +%s)
set +e
cargo "$@"
rc=$?
echo "exit=$rc elapsed=$(( $(date +%s) - start ))s"
exit $rc
