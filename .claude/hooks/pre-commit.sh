#!/bin/bash
# Git Pre-Commit Hook (Items 7, 8, 9)
# BLOCKING: YES — rejects commits with placeholders, broken spec syntax, or dep violations
#
# This script is installed into .git/hooks/pre-commit by scripts/setup-hooks.sh
set -euo pipefail

WORKSPACE_ROOT="$(git rev-parse --show-toplevel)"

# --- Item 7: Integrity Check (Placeholder Detection) ---
echo "Pre-commit: checking for placeholders in staged files..." >&2

STAGED_RS="$(git diff --cached --name-only --diff-filter=ACM -- '*.rs' | grep -v '/tests/' | grep -v '_test\.rs' | grep -v 'test_' || true)"

if [ -n "$STAGED_RS" ]; then
    INTEGRITY_FAIL=false

    for FILE in $STAGED_RS; do
        # Only check staged content (not working tree)
        STAGED_CONTENT="$(git show ":$FILE" 2>/dev/null || true)"
        [ -z "$STAGED_CONTENT" ] && continue

        ISSUES=""

        # Check for TODO/FIXME/HACK/XXX
        if echo "$STAGED_CONTENT" | grep -nE '(TODO|FIXME|XXX|HACK)' | grep -qv '^[[:space:]]*//' 2>/dev/null; then
            LINE_INFO="$(echo "$STAGED_CONTENT" | grep -nE '(TODO|FIXME|XXX|HACK)' | head -3)"
            ISSUES="${ISSUES}\n  TODO/FIXME/HACK:\n${LINE_INFO}"
        fi

        # Check for unimplemented!()/todo!()
        if echo "$STAGED_CONTENT" | grep -nE 'unimplemented!\(\)|todo!\(\)' 2>/dev/null | grep -qv '^[[:space:]]*//' ; then
            LINE_INFO="$(echo "$STAGED_CONTENT" | grep -nE 'unimplemented!\(\)|todo!\(\)' | head -3)"
            ISSUES="${ISSUES}\n  unimplemented!/todo!:\n${LINE_INFO}"
        fi

        # Check for panic!("not implemented")
        if echo "$STAGED_CONTENT" | grep -nE 'panic!\("not implemented' 2>/dev/null | grep -qv '^[[:space:]]*//' ; then
            LINE_INFO="$(echo "$STAGED_CONTENT" | grep -nE 'panic!\("not implemented' | head -3)"
            ISSUES="${ISSUES}\n  panic!(\"not implemented\"):\n${LINE_INFO}"
        fi

        # Check for unwrap() in non-test code
        if echo "$STAGED_CONTENT" | grep -nE '\.unwrap\(\)' 2>/dev/null | grep -qv '^[[:space:]]*//' | grep -qv '#\[test\]' ; then
            LINE_INFO="$(echo "$STAGED_CONTENT" | grep -nE '\.unwrap\(\)' | head -3)"
            ISSUES="${ISSUES}\n  unwrap() calls:\n${LINE_INFO}"
        fi

        if [ -n "$ISSUES" ]; then
            echo "BLOCKED: Placeholders found in $FILE:" >&2
            echo -e "$ISSUES" >&2
            INTEGRITY_FAIL=true
        fi
    done

    if [ "$INTEGRITY_FAIL" = true ]; then
        echo "" >&2
        echo "Remove TODO/FIXME/HACK comments, unimplemented!(), todo!(), and unwrap() from production code." >&2
        echo "Use proper error handling (?, .map_err(), .expect(\"reason\"))." >&2
        exit 1
    fi
fi

# --- Item 8: Spec Syntax Validation ---
STAGED_SPECS="$(git diff --cached --name-only --diff-filter=ACM -- '*.ioa.toml' || true)"

if [ -n "$STAGED_SPECS" ]; then
    echo "Pre-commit: validating spec syntax..." >&2

    for SPEC in $STAGED_SPECS; do
        # Try parsing the spec (syntax check only, not full cascade)
        if ! cargo run -p temper-cli --quiet -- verify --specs-dir "$(dirname "$SPEC")" 2>/dev/null; then
            echo "BLOCKED: Spec syntax error in $SPEC" >&2
            echo "Fix the spec before committing." >&2
            exit 1
        fi
    done
fi

# --- Item 9: Dependency Audit ---
STAGED_CARGO="$(git diff --cached --name-only --diff-filter=ACM -- 'Cargo.toml' '**/Cargo.toml' || true)"

if [ -n "$STAGED_CARGO" ]; then
    echo "Pre-commit: running dependency audit..." >&2

    if [ -x "$WORKSPACE_ROOT/scripts/audit-deps.sh" ]; then
        if ! "$WORKSPACE_ROOT/scripts/audit-deps.sh" >&2; then
            echo "BLOCKED: Dependency isolation violated." >&2
            exit 1
        fi
    fi
fi

echo "Pre-commit: OK" >&2
exit 0
