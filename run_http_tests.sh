#!/usr/bin/env bash
# Run the Traverse HTTP client tests against a running server
# Usage: ./run_http_tests.sh [python|node|go|java|all]
#
# The tests read TRAVERSE_BASE_URL (default http://localhost:7691) and TRAVERSE_API_KEY (when the
# server has auth on); the server needs TRAVERSE_DATA pointing at a directory.
# Python runs in the venv at python/http/.venv unless PYTHON names an interpreter that has the
# client installed. Other tools are taken from PATH; override with NODE, GO or MVN.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PASSED=0
FAILED=0
SKIPPED=0
RESULTS=()

run_python() {
    echo "=== Python HTTP ==="
    local py="${PYTHON:-}"
    if [ -z "$py" ]; then
        local venv="$SCRIPT_DIR/python/http/.venv" candidate
        for candidate in "$venv/Scripts/python.exe" "$venv/bin/python"; do
            if [ -x "$candidate" ]; then
                py="$candidate"
                break
            fi
        done
    fi
    if [ -z "$py" ]; then
        echo "SKIP: no venv at python/http/.venv (create one: python -m venv python/http/.venv, then install the client into it: pip install -e \"python/http[dev]\")"
        SKIPPED=$((SKIPPED + 1))
        RESULTS+=("Python HTTP: SKIPPED (no venv)")
        return
    fi
    if (cd "$SCRIPT_DIR/python/http" && "$py" -m pytest tests/ -v); then
        PASSED=$((PASSED + 1))
        RESULTS+=("Python HTTP: PASSED")
    else
        FAILED=$((FAILED + 1))
        RESULTS+=("Python HTTP: FAILED")
    fi
}

run_node() {
    echo "=== Node.js HTTP ==="
    local node_cmd="${NODE:-node}"
    if ! "$node_cmd" --version &>/dev/null; then
        echo "SKIP: node not found"
        SKIPPED=$((SKIPPED + 1))
        RESULTS+=("Node.js HTTP: SKIPPED")
        return
    fi
    if (cd "$SCRIPT_DIR/node/http" && "$node_cmd" --test tests/client.test.js); then
        PASSED=$((PASSED + 1))
        RESULTS+=("Node.js HTTP: PASSED")
    else
        FAILED=$((FAILED + 1))
        RESULTS+=("Node.js HTTP: FAILED")
    fi
}

run_go() {
    echo "=== Go HTTP ==="
    local go_cmd="${GO:-go}"
    if ! command -v "$go_cmd" &>/dev/null && ! [ -x "$go_cmd" ]; then
        echo "SKIP: go not found"
        SKIPPED=$((SKIPPED + 1))
        RESULTS+=("Go HTTP: SKIPPED")
        return
    fi
    if (cd "$SCRIPT_DIR/go/http" && "$go_cmd" test -v -count=1 ./...); then
        PASSED=$((PASSED + 1))
        RESULTS+=("Go HTTP: PASSED")
    else
        FAILED=$((FAILED + 1))
        RESULTS+=("Go HTTP: FAILED")
    fi
}

run_java() {
    echo "=== Java HTTP ==="
    local mvn_cmd="${MVN:-mvn}"
    if ! command -v "$mvn_cmd" &>/dev/null && ! [ -x "$mvn_cmd" ]; then
        echo "SKIP: maven not found"
        SKIPPED=$((SKIPPED + 1))
        RESULTS+=("Java HTTP: SKIPPED")
        return
    fi
    if (cd "$SCRIPT_DIR/java/http" && "$mvn_cmd" test -q); then
        PASSED=$((PASSED + 1))
        RESULTS+=("Java HTTP: PASSED")
    else
        FAILED=$((FAILED + 1))
        RESULTS+=("Java HTTP: FAILED")
    fi
}

target="${1:-all}"

case "$target" in
    python)  run_python ;;
    node)    run_node ;;
    go)      run_go ;;
    java)    run_java ;;
    all)
        run_python
        echo ""
        run_node
        echo ""
        run_go
        echo ""
        run_java
        ;;
    *)
        echo "Usage: $0 [python|node|go|java|all]"
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
