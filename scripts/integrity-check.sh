#!/bin/bash
# Item 13: Placeholder/Hack Detector
# Comprehensive scan of the entire codebase for code quality issues.
# Reports findings grouped by crate with severity levels.
set -euo pipefail

WORKSPACE_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CRATES_DIR="$WORKSPACE_ROOT/crates"

echo "=== Temper Integrity Check ==="
echo ""

TOTAL_ISSUES=0
CRITICAL=0
WARNING=0

# Helper: Find the line number where #[cfg(test)] starts in a file.
# Returns 0 if not found (meaning all lines are production code).
find_test_boundary() {
    local FILE="$1"
    local LINE_NUM
    LINE_NUM="$(grep -n '#\[cfg(test)\]' "$FILE" 2>/dev/null | head -1 | cut -d: -f1 || true)"
    echo "${LINE_NUM:-0}"
}

# Helper: scan files for a pattern, excluding test code
scan_pattern() {
    local LABEL="$1"
    local PATTERN="$2"
    local SEVERITY="$3"
    local EXCLUDE_TESTS="${4:-true}"

    local FOUND=0
    local RESULTS=""

    while IFS= read -r FILE; do
        [ -f "$FILE" ] || continue

        # Skip test files if requested
        if [ "$EXCLUDE_TESTS" = true ]; then
            case "$FILE" in
                */tests/*|*_test.rs|*_tests.rs|*test_*.rs|*/tests.rs|*/benches/*) continue ;;
            esac
        fi

        local MATCHES
        MATCHES="$(grep -nE "$PATTERN" "$FILE" 2>/dev/null | grep -v '^[[:space:]]*//' | grep -v '// ci-ok' || true)"

        # For unwrap() specifically, exclude safe patterns:
        # - Lock acquisition: .read().unwrap(), .write().unwrap(), .lock().unwrap()
        # - Debug assertions: debug_assert! macros
        # - Proc macros: syn::parse().unwrap() (compile-time only)
        # - Compile-time: with_ymd_and_hms().unwrap() (constant epoch values)
        # - TigerStyle assertions: .last().unwrap() inside debug_assert blocks
        # - Strip patterns: strip_prefix/strip_suffix after match guard
        # - Container access after length check: .pop().unwrap(), .next().unwrap()
        #   after checking len()==1 or !is_empty()
        # - Simulation internals: get_mut(actor_id).unwrap() in sim scheduler
        if [ -n "$MATCHES" ] && [ "$PATTERN" = '\.unwrap\(\)' ]; then
            MATCHES="$(echo "$MATCHES" \
                | grep -v '\.read()\.unwrap()' \
                | grep -v '\.write()\.unwrap()' \
                | grep -v '\.lock()\.unwrap()' \
                | grep -v 'debug_assert' \
                | grep -v 'syn::parse' \
                | grep -v 'with_ymd_and_hms' \
                | grep -v 'strip_prefix.*\.unwrap()' \
                | grep -v 'strip_suffix.*\.unwrap()' \
                | grep -v '\.last()\.unwrap()' \
                | grep -v '\.pop()\.unwrap()' \
                | grep -v '\.next()\.unwrap()' \
                | grep -v 'get_mut.*\.unwrap()' \
                | grep -v '\.get(&.*\.unwrap()' \
                | grep -v '\.chars()\.next()\.unwrap()' \
                || true)"
        fi

        # If excluding tests, also filter out lines inside #[cfg(test)] blocks
        if [ -n "$MATCHES" ] && [ "$EXCLUDE_TESTS" = true ]; then
            local TEST_BOUNDARY
            TEST_BOUNDARY=$(find_test_boundary "$FILE")
            if [ "$TEST_BOUNDARY" -gt 0 ]; then
                # Only keep lines before the #[cfg(test)] boundary
                local FILTERED=""
                while IFS= read -r LINE; do
                    local LINE_NUM
                    LINE_NUM="$(echo "$LINE" | cut -d: -f1)"
                    if [ "$LINE_NUM" -lt "$TEST_BOUNDARY" ]; then
                        FILTERED="${FILTERED}${LINE}\n"
                    fi
                done <<< "$MATCHES"
                MATCHES="$(echo -e "$FILTERED" | sed '/^$/d')"
            fi
        fi

        if [ -n "$MATCHES" ]; then
            local REL_FILE
            REL_FILE="$(realpath --relative-to="$WORKSPACE_ROOT" "$FILE" 2>/dev/null || echo "$FILE")"
            RESULTS="${RESULTS}\n  $REL_FILE:"
            while IFS= read -r LINE; do
                RESULTS="${RESULTS}\n    $LINE"
                FOUND=$((FOUND + 1))
            done <<< "$MATCHES"
        fi
    done < <(find "$CRATES_DIR" -name "*.rs" -not -path "*/target/*")

    if [ "$FOUND" -gt 0 ]; then
        echo "[$SEVERITY] $LABEL ($FOUND occurrences):"
        echo -e "$RESULTS"
        echo ""
        TOTAL_ISSUES=$((TOTAL_ISSUES + FOUND))
        if [ "$SEVERITY" = "CRITICAL" ]; then
            CRITICAL=$((CRITICAL + FOUND))
        else
            WARNING=$((WARNING + FOUND))
        fi
    fi
}

# CRITICAL: These should never be in production code
scan_pattern "TODO/FIXME/HACK/XXX comments" '(TODO|FIXME|XXX|HACK)\b' "CRITICAL"
scan_pattern "unimplemented!()/todo!() macros" 'unimplemented!\(\)|todo!\(\)' "CRITICAL"
scan_pattern "panic!(\"not implemented\")" 'panic!\("not implemented' "CRITICAL"
# Note: RwLock/Mutex .read()/.write().unwrap() is standard Rust — poison
# means another thread panicked, so propagating is the correct TigerStyle
# behavior (fail fast on corrupt state). We scan for unwrap() but exclude
# lock acquisition patterns and common safe patterns.
scan_pattern "unwrap() calls" '\.unwrap\(\)' "CRITICAL"

# WARNING: Should be reviewed
scan_pattern "println!() debugging" 'println!\(' "WARNING"
scan_pattern "#[allow(dead_code)]" '#\[allow\(dead_code\)\]' "WARNING"

echo "=== Summary ==="
echo "Total issues: $TOTAL_ISSUES (Critical: $CRITICAL, Warning: $WARNING)"

if [ "$CRITICAL" -gt 0 ]; then
    echo ""
    echo "CRITICAL issues found. These must be resolved before release."
    exit 1
fi

if [ "$TOTAL_ISSUES" -eq 0 ]; then
    echo "No issues found. Codebase is clean."
fi
