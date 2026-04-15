#!/bin/bash
# Item 12: Full Cascade Runner
# Runs temper verify on all spec directories, captures JSON results with timestamps.
# Results stored in .cascade-results/ for regression tracking.
set -euo pipefail

WORKSPACE_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
RESULTS_DIR="$WORKSPACE_ROOT/.cascade-results"
TIMESTAMP="$(date -u +%Y%m%d_%H%M%S)"

echo "=== Temper Verification Cascade ==="
echo "Timestamp: $TIMESTAMP"
echo ""

mkdir -p "$RESULTS_DIR"

# Find all spec directories (contain .ioa.toml files)
SPEC_DIRS="$(find "$WORKSPACE_ROOT" -name "*.ioa.toml" -not -path "*/target/*" -exec dirname {} \; | sort -u)"

if [ -z "$SPEC_DIRS" ]; then
    echo "No .ioa.toml specs found."
    exit 0
fi

TOTAL=0
PASSED=0
FAILED=0
RESULTS_FILE="$RESULTS_DIR/cascade_${TIMESTAMP}.json"

# Start JSON array
echo "[" > "$RESULTS_FILE"
FIRST=true

for DIR in $SPEC_DIRS; do
    TOTAL=$((TOTAL + 1))
    SPEC_NAME="$(basename "$DIR")"
    REL_DIR="$(realpath --relative-to="$WORKSPACE_ROOT" "$DIR" 2>/dev/null || echo "$DIR")"

    echo "Verifying: $REL_DIR"

    # Run verification and capture output
    VERIFY_OUTPUT=""
    VERIFY_EXIT=0
    VERIFY_OUTPUT="$(cd "$WORKSPACE_ROOT" && cargo run -p temper-cli --quiet -- verify --specs-dir "$DIR" 2>&1)" || VERIFY_EXIT=$?

    STATUS="pass"
    if [ "$VERIFY_EXIT" -ne 0 ]; then
        STATUS="fail"
        FAILED=$((FAILED + 1))
        echo "  FAILED (exit $VERIFY_EXIT)"
    else
        PASSED=$((PASSED + 1))
        echo "  PASSED"
    fi

    # Append to JSON results
    if [ "$FIRST" = true ]; then
        FIRST=false
    else
        echo "," >> "$RESULTS_FILE"
    fi

    # Escape output for JSON
    ESCAPED_OUTPUT="$(echo "$VERIFY_OUTPUT" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))' 2>/dev/null || echo '""')"

    cat >> "$RESULTS_FILE" << EOF
  {
    "spec": "$SPEC_NAME",
    "directory": "$REL_DIR",
    "status": "$STATUS",
    "exit_code": $VERIFY_EXIT,
    "timestamp": "$TIMESTAMP",
    "output": $ESCAPED_OUTPUT
  }
EOF
done

echo "]" >> "$RESULTS_FILE"

echo ""
echo "=== Cascade Results ==="
echo "Total: $TOTAL | Passed: $PASSED | Failed: $FAILED"
echo "Results saved: $RESULTS_FILE"

if [ "$FAILED" -gt 0 ]; then
    exit 1
fi
