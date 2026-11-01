#!/usr/bin/env bash
# WASM smoke-test runner for traverse-ml-wasm.
#
# Builds the wasm_predict test binary and runs it through
# wasm-bindgen-test-runner in Node.js. Verifies:
#   1. The tiny_gcn.tvmodel fixture (committed in tests/fixtures/)
#      bincode-decodes into a `ModelHandle` in WASM.
#   2. `predict_node_classification_js` runs on the same graph
#      the fixture was trained on and recovers the training labels.
#
# Regenerate the fixture if the Model schema changes:
#   cargo run --example gen_fixture -p traverse-ml-wasm
#
# Prerequisites:
#   * Rust + wasm32-unknown-unknown target.
#   * wasm-pack (provides the test runner).
#   * Node.js (the runner uses it for the test harness).
#
# Same Windows cwd quirk as the traverse-ml smoke scripts: build
# first, then invoke the runner directly on the .wasm artifact.

set -euo pipefail

cd "$(dirname "$0")/../.."
CARGO_CMD="${CARGO:-cargo}"

echo "--- Building WASM predict test (cpu backend) -----------"
"$CARGO_CMD" build --target wasm32-unknown-unknown \
    --no-default-features --features cpu \
    --tests -p traverse-ml-wasm --test wasm_predict 2>&1

# cargo's target directory (CARGO_TARGET_DIR or ~/.cargo/config.toml may put it outside the checkout)
TARGET_DIR="$("$CARGO_CMD" metadata --format-version 1 --no-deps \
    | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p' | sed 's|\\\\|/|g')"

WASM=$(ls -t "$TARGET_DIR"/wasm32-unknown-unknown/debug/deps/wasm_predict-*.wasm | head -1)
echo "--- Test artifact: ${WASM} -----------------------------"

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

echo "--- Runner: ${RUNNER} ----------------------------------"
echo "--- Running predict smoke tests in Node.js -------------"
"$RUNNER" "$WASM"
