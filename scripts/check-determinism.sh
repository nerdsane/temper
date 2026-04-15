#!/bin/bash
# Item 14: Determinism Audit
# Full codebase scan for non-deterministic patterns in simulation-visible code.
set -euo pipefail

WORKSPACE_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

echo "=== Determinism Audit ==="
echo "Checking simulation-visible crates for non-deterministic patterns..."
echo ""

TOTAL_ISSUES=0

# Simulation-visible crates
SIM_CRATES="temper-runtime temper-jit temper-server"

scan_nondeterminism() {
    local LABEL="$1"
    local PATTERN="$2"
    local SUGGESTION="$3"

    local FOUND=0
    local RESULTS=""

    for CRATE in $SIM_CRATES; do
        CRATE_DIR="$WORKSPACE_ROOT/crates/$CRATE"
        [ -d "$CRATE_DIR" ] || continue

        while IFS= read -r FILE; do
            [ -f "$FILE" ] || continue

            # Skip test files
            case "$FILE" in
                */tests/*|*_test.rs|*test_*.rs|*/benches/*) continue ;;
            esac

            local MATCHES
            MATCHES="$(grep -nE "$PATTERN" "$FILE" 2>/dev/null | grep -v '// determinism-ok' | grep -v '^[[:space:]]*//' || true)"
            if [ -n "$MATCHES" ]; then
                local REL_FILE
                REL_FILE="$(realpath --relative-to="$WORKSPACE_ROOT" "$FILE" 2>/dev/null || echo "$FILE")"
                RESULTS="${RESULTS}\n  $REL_FILE:"
                while IFS= read -r LINE; do
                    RESULTS="${RESULTS}\n    $LINE"
                    FOUND=$((FOUND + 1))
                done <<< "$MATCHES"
            fi
        done < <(find "$CRATE_DIR/src" -name "*.rs" 2>/dev/null)
    done

    if [ "$FOUND" -gt 0 ]; then
        echo "$LABEL ($FOUND occurrences) — $SUGGESTION:"
        echo -e "$RESULTS"
        echo ""
        TOTAL_ISSUES=$((TOTAL_ISSUES + FOUND))
    fi
}

scan_nondeterminism \
    "HashMap usage" \
    'HashMap' \
    "Use BTreeMap for deterministic iteration order"

scan_nondeterminism \
    "Wall-clock time" \
    'SystemTime::now\(\)|Instant::now\(\)' \
    "Use sim_now() for simulation-safe time"

scan_nondeterminism \
    "Random UUIDs" \
    'Uuid::new_v4\(\)' \
    "Use sim_uuid() for deterministic UUIDs"

scan_nondeterminism \
    "Unseeded RNG" \
    'thread_rng\(\)|rand::random' \
    "Use seeded RNG for reproducible simulation"

echo "=== Summary ==="
echo "Total non-deterministic patterns: $TOTAL_ISSUES"

if [ "$TOTAL_ISSUES" -gt 0 ]; then
    echo ""
    echo "Add '// determinism-ok' comment to suppress false positives."
    echo "These patterns break DST (L2 verification) reproducibility."
fi
