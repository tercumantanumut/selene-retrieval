#!/usr/bin/env bash
# Builds both spike packages into spikes/ipc-bridge/target (Windows-style path so cargo
# does not resolve "/c/..." to "C:\c\...").
set -euo pipefail
export PATH="$HOME/.cargo/bin:$PATH"
S="$(cd "$(dirname "$0")/.." && pwd -W)"
export CARGO_TARGET_DIR="$S/target"
(cd "$S/bridge" && cargo build --release "$@")
(cd "$S/daemon" && cargo build --release "$@")
ls -la "$CARGO_TARGET_DIR/release/"*.exe
