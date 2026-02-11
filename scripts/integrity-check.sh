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
                */tests/*|*_test.rs|*test_*.rs|*/benches/*) continue ;;
            esac
        fi

        local MATCHES
        MATCHES="$(grep -nE "$PATTERN" "$FILE" 2>/dev/null | grep -v '^[[:space:]]*//' || true)"
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
