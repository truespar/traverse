#!/usr/bin/env bash
# WASM smoke-test runner for traverse-ml: the inference path must actually run
# in a WASM runtime (the native `cargo test` suite relies on std::fs, tempfile
# and threads, none of which exist in browser WASM).
#
# Verifies:
#   1. the crate compiles for wasm32-unknown-unknown with the CPU (NdArray +
#      autodiff) backend;
#   2. CSR graph construction + normalization runs;
#   3. Burn NdArray tensors instantiate and compute correctly;
#   4. a full 2-layer GCN forward pass gives finite output.
#
# Prerequisites:
#   * Rust with the wasm32-unknown-unknown target
#     (`rustup target add wasm32-unknown-unknown`)
#   * wasm-pack (installs wasm-bindgen-test-runner under ~/.wasm-pack/ or
#     %LOCALAPPDATA%\.wasm-pack\)
#   * Node.js (the runner's harness)
#
# Usage:
#   ./crates/traverse-ml/run_wasm_smoke.sh
#
# The Windows test runner has a cwd quirk that breaks cargo's normal
# invocation ("Access is denied"), so build first, then run the runner
# directly on the .wasm artifact.

set -euo pipefail

cd "$(dirname "$0")/../.."
CARGO_CMD="${CARGO:-cargo}"

echo "--- Building WASM test binary (cpu + autodiff) ---------"
"$CARGO_CMD" build --target wasm32-unknown-unknown \
    --no-default-features --features cpu \
    --tests -p traverse-ml --test wasm_smoke 2>&1

# cargo's target directory (CARGO_TARGET_DIR or ~/.cargo/config.toml may put it outside the checkout)
TARGET_DIR="$("$CARGO_CMD" metadata --format-version 1 --no-deps \
    | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p' | sed 's|\\\\|/|g')"

# freshest wasm_smoke artifact
WASM=$(ls -t "$TARGET_DIR"/wasm32-unknown-unknown/debug/deps/wasm_smoke-*.wasm | head -1)
echo "--- Test artifact: ${WASM} -----------------------------"

# locate the wasm-bindgen-test-runner wasm-pack installed
RUNNER=""
for candidate in \
    "$HOME/.wasm-pack/wasm-bindgen-"*/wasm-bindgen-test-runner.exe \
    "$HOME/.wasm-pack/wasm-bindgen-"*/wasm-bindgen-test-runner \
    "${LOCALAPPDATA:-}/.wasm-pack/wasm-bindgen-"*/wasm-bindgen-test-runner.exe; do
    if [[ -x "$candidate" ]]; then
        RUNNER="$candidate"
        break
    fi
done

if [[ -z "$RUNNER" ]]; then
    echo "ERROR: wasm-bindgen-test-runner not found. Did wasm-pack install correctly?" >&2
    exit 1
fi

echo "--- Runner: ${RUNNER} ----------------------------------"
echo "--- Running WASM smoke tests ---------------------------"
"$RUNNER" "$WASM"
