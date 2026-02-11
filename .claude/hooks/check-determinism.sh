#!/bin/bash
# Item 4: Determinism Guard (PostToolUse — Write|Edit)
# BLOCKING: No (advisory only, exit 0 always)
#
# After editing .rs files in simulation-visible crates, scans for
# non-deterministic patterns that break DST reproducibility.
set -euo pipefail

PAYLOAD="$(cat)"

# Extract file path from tool input
if command -v jq >/dev/null 2>&1; then
    FILE_PATH="$(echo "$PAYLOAD" | jq -r '.tool_input.file_path // .tool_input.path // empty')"
else
    FILE_PATH="$(echo "$PAYLOAD" | grep -o '"file_path"[[:space:]]*:[[:space:]]*"[^"]*"' | head -1 | sed 's/.*"\([^"]*\)"$/\1/' || true)"
fi

# Only check .rs files
case "${FILE_PATH:-}" in
    *.rs) ;;
    *) exit 0 ;;
esac

# Only check simulation-visible crates
case "${FILE_PATH:-}" in
    */temper-runtime/*|*/temper-jit/*|*/temper-server/*) ;;
    *) exit 0 ;;
esac

# Skip test files
case "${FILE_PATH:-}" in
    */tests/*|*_test.rs|*test_*.rs) exit 0 ;;
esac

# Check for non-deterministic patterns
WARNINGS=""

if grep -n 'HashMap' "$FILE_PATH" 2>/dev/null | grep -v '// determinism-ok' | grep -v 'BTreeMap' | grep -qv '^[[:space:]]*//' ; then
    WARNINGS="${WARNINGS}\n  - HashMap found (use BTreeMap for deterministic iteration)"
fi

if grep -n 'SystemTime::now\|Instant::now' "$FILE_PATH" 2>/dev/null | grep -v '// determinism-ok' | grep -qv '^[[:space:]]*//' ; then
    WARNINGS="${WARNINGS}\n  - SystemTime::now()/Instant::now() found (use sim_now())"
fi

if grep -n 'Uuid::new_v4' "$FILE_PATH" 2>/dev/null | grep -v '// determinism-ok' | grep -qv '^[[:space:]]*//' ; then
    WARNINGS="${WARNINGS}\n  - Uuid::new_v4() found (use sim_uuid())"
fi

if grep -n 'thread_rng\|rand::random' "$FILE_PATH" 2>/dev/null | grep -v '// determinism-ok' | grep -qv '^[[:space:]]*//' ; then
    WARNINGS="${WARNINGS}\n  - thread_rng()/rand::random() found (use seeded RNG)"
fi

if [ -n "$WARNINGS" ]; then
    echo "DETERMINISM WARNING in $(basename "$FILE_PATH"):" >&2
    echo -e "$WARNINGS" >&2
    echo "Add '// determinism-ok' comment to suppress false positives." >&2
fi

exit 0
