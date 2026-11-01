#!/usr/bin/env bash
# Run the Traverse embedded binding tests
# Usage: ./run_binding_tests.sh [python|java|node|go|dotnet|all]
#
# Java, Node.js, Go and .NET test the native library cargo builds from this checkout (a debug
# build, made here first): TRAVERSE_JAVA_LIB, NAPI_RS_NATIVE_LIBRARY_PATH and TRAVERSE_FFI_LIB
# point them at it. Python tests the installed module (maturin develop).
# Tools are taken from PATH; override with CARGO, PYTHON, MVN, NODE, GO or DOTNET.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PASSED=0
FAILED=0
SKIPPED=0
RESULTS=()

CARGO_CMD="${CARGO:-cargo}"

# file name of a cdylib on this platform
lib_file() {
    case "$(uname -s)" in
        MINGW* | MSYS* | CYGWIN*) echo "$1.dll" ;;
        Darwin) echo "lib$1.dylib" ;;
        *) echo "lib$1.so" ;;
    esac
}

# cargo's debug output directory (~/.cargo/config.toml may put the target dir anywhere)
debug_dir() {
    local target
    target="$(cd "$SCRIPT_DIR" && "$CARGO_CMD" metadata --format-version 1 --no-deps \
        | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p' | sed 's|\\\\|/|g')"
    echo "$target/debug"
}

# builds `crate` and prints its library's path; empty when the build failed
build_lib() {
    local crate="$1" name="$2"
    if ! (cd "$SCRIPT_DIR" && "$CARGO_CMD" build -p "$crate" >&2); then
        return
    fi
    echo "$(debug_dir)/$(lib_file "$name")"
}

record() {
    local name="$1" outcome="$2"
    case "$outcome" in
        PASSED) PASSED=$((PASSED + 1)) ;;
        FAILED*) FAILED=$((FAILED + 1)) ;;
        *) SKIPPED=$((SKIPPED + 1)) ;;
    esac
    RESULTS+=("$name: $outcome")
}

run_python() {
    echo "=== Python (PyO3) ==="
    local py_cmd="${PYTHON:-python}"
    if ! "$py_cmd" --version &>/dev/null; then
        echo "SKIP: python not found"
        record Python SKIPPED
        return
    fi
    if ! "$py_cmd" -c "import traverse" &>/dev/null; then
        echo "SKIP: traverse module not installed (run: pip install crates/traverse-python)"
        record Python "SKIPPED (module not installed)"
        return
    fi
    if (cd "$SCRIPT_DIR/python" && "$py_cmd" -m pytest tests/ -v); then
        record Python PASSED
    else
        record Python FAILED
    fi
}

run_java() {
    echo "=== Java (JNI) ==="
    local mvn_cmd="${MVN:-mvn}"
    if ! command -v "$mvn_cmd" &>/dev/null && ! [ -x "$mvn_cmd" ]; then
        echo "SKIP: maven not found"
        record Java SKIPPED
        return
    fi
    local lib
    lib="$(build_lib traverse-java traverse_java)"
    if [ -z "$lib" ]; then
        record Java "FAILED (cargo build)"
        return
    fi
    if (cd "$SCRIPT_DIR/java" && TRAVERSE_JAVA_LIB="$lib" "$mvn_cmd" test -q); then
        record Java PASSED
    else
        record Java FAILED
    fi
}

run_node() {
    echo "=== Node.js (napi-rs) ==="
    local node_cmd="${NODE:-node}"
    if ! "$node_cmd" --version &>/dev/null; then
        echo "SKIP: node not found"
        record Node.js SKIPPED
        return
    fi
    local lib
    lib="$(build_lib traverse-node traverse_node)"
    if [ -z "$lib" ]; then
        record Node.js "FAILED (cargo build)"
        return
    fi
    # Node.js loads an addon only from a .node file
    local addon="${lib%.*}.node"
    cp "$lib" "$addon"
    if (cd "$SCRIPT_DIR/node/embedded" && NAPI_RS_NATIVE_LIBRARY_PATH="$addon" "$node_cmd" tests/traverse.test.js); then
        record Node.js PASSED
    else
        record Node.js FAILED
    fi
}

run_go() {
    echo "=== Go (FFI, no CGo) ==="
    local go_cmd="${GO:-go}"
    if ! command -v "$go_cmd" &>/dev/null && ! [ -x "$go_cmd" ]; then
        echo "SKIP: go not found"
        record Go SKIPPED
        return
    fi
    local lib
    lib="$(build_lib traverse-ffi traverse_ffi)"
    if [ -z "$lib" ]; then
        record Go "FAILED (cargo build)"
        return
    fi
    if (cd "$SCRIPT_DIR/go" && TRAVERSE_FFI_LIB="$lib" CGO_ENABLED=0 "$go_cmd" test -count=1 ./...); then
        record Go PASSED
    else
        record Go FAILED
    fi
}

run_dotnet() {
    echo "=== .NET (FFI) ==="
    local dotnet_cmd="${DOTNET:-dotnet}"
    if ! command -v "$dotnet_cmd" &>/dev/null && ! [ -x "$dotnet_cmd" ]; then
        echo "SKIP: dotnet not found"
        record .NET SKIPPED
        return
    fi
    local lib
    lib="$(build_lib traverse-ffi traverse_ffi)"
    if [ -z "$lib" ]; then
        record .NET "FAILED (cargo build)"
        return
    fi
    # global.json there selects Microsoft.Testing.Platform for `dotnet test`
    if (cd "$SCRIPT_DIR/dotnet" && TRAVERSE_FFI_LIB="$lib" "$dotnet_cmd" test --project test/Traverse.Embedded.Tests); then
        record .NET PASSED
    else
        record .NET FAILED
    fi
}

target="${1:-all}"

case "$target" in
    python)  run_python ;;
    java)    run_java ;;
    node)    run_node ;;
    go)      run_go ;;
    dotnet)  run_dotnet ;;
    all)
        run_python
        echo ""
        run_java
        echo ""
        run_node
        echo ""
        run_go
        echo ""
        run_dotnet
        ;;
    *)
        echo "Usage: $0 [python|java|node|go|dotnet|all]"
        exit 1
        ;;
esac

echo ""
echo "=== Summary ==="
for r in "${RESULTS[@]}"; do
    echo "  $r"
done
echo "Passed: $PASSED  Failed: $FAILED  Skipped: $SKIPPED"

[ "$FAILED" -eq 0 ]
