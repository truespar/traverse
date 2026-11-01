#!/usr/bin/env bash
# WGPU-backed WASM smoke-test runner for traverse-ml; the companion of
# `run_wasm_smoke.sh` (CPU / NdArray under Node). Runs the production target:
# Burn's WGPU backend in a real browser via WebGPU.
#
# Verifies:
#   1. the crate compiles for wasm32-unknown-unknown with `wgpu`;
#   2. Burn's WGPU runtime initializes in a browser tab (navigator.gpu,
#      adapter + device);
#   3. a small tensor op round-trips through WebGPU;
#   4. a 2-layer GCN forward pass gives finite output.
#
# Prerequisites:
#   * Rust with the wasm32-unknown-unknown target
#   * wasm-pack (test runner + chromedriver)
#   * Chrome 113+ with WebGPU (default on Windows / macOS; Linux may need
#     `chrome://flags/#enable-unsafe-webgpu`)
#   * GPU drivers, even headless: WebGPU won't start without a real adapter
#
# Usage:
#   ./crates/traverse-ml/run_wasm_smoke_wgpu.sh
#
# Same Windows cwd quirk as `run_wasm_smoke.sh`: build first, then run the
# runner directly on the .wasm artifact.

set -euo pipefail

cd "$(dirname "$0")/../.."
CARGO_CMD="${CARGO:-cargo}"

echo "--- Building WASM test binary (wgpu) -------------------"
"$CARGO_CMD" build --target wasm32-unknown-unknown \
    --no-default-features --features wgpu \
    --tests -p traverse-ml --test wasm_smoke_wgpu 2>&1

# cargo's target directory (CARGO_TARGET_DIR or ~/.cargo/config.toml may put it outside the checkout)
TARGET_DIR="$("$CARGO_CMD" metadata --format-version 1 --no-deps \
    | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p' | sed 's|\\\\|/|g')"

WASM=$(ls -t "$TARGET_DIR"/wasm32-unknown-unknown/debug/deps/wasm_smoke_wgpu-*.wasm | head -1)
echo "--- Test artifact: ${WASM} -----------------------------"

# locate wasm-bindgen-test-runner
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
    echo "ERROR: wasm-bindgen-test-runner not found." >&2
    exit 1
fi

# browser mode + headless (the runner reads these to pick the driver); an empty CHROMEDRIVER_REMOTE
# breaks URL parsing, so it stays unset and CHROMEDRIVER below is used
export WASM_BINDGEN_USE_BROWSER=1
export WASM_BINDGEN_TEST_TIMEOUT=120
unset CHROMEDRIVER_REMOTE 2>/dev/null || true

# locate chromedriver; wasm-bindgen-test-runner doesn't ship one, use the one from `npm i -g
# chromedriver` (matches the installed Chrome)
if [[ -z "${CHROMEDRIVER:-}" ]]; then
    for cand in \
        "$HOME/AppData/Roaming/npm/node_modules/chromedriver/lib/chromedriver/chromedriver.exe" \
        "$(npm root -g 2>/dev/null)/chromedriver/lib/chromedriver/chromedriver.exe" \
        "$(npm root -g 2>/dev/null)/chromedriver/lib/chromedriver/chromedriver" \
        "$(which chromedriver 2>/dev/null || true)"; do
        if [[ -n "$cand" && -x "$cand" ]]; then
            export CHROMEDRIVER="$cand"
            break
        fi
    done
fi
if [[ -z "${CHROMEDRIVER:-}" ]]; then
    echo "ERROR: chromedriver not found. Install with: npm i -g chromedriver" >&2
    exit 1
fi
echo "--- chromedriver: ${CHROMEDRIVER} ----------------------"

echo "--- Runner: ${RUNNER} ----------------------------------"
echo "--- Running WGPU smoke tests in Chrome (headless) ------"
echo "    Requires Chrome 113+ with WebGPU enabled."
"$RUNNER" "$WASM"
