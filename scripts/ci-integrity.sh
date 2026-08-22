#!/bin/bash
# Run the repository's complete non-test CI integrity contract.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

FOUND=false
while IFS= read -r file; do
    production="$(awk '/^#\[cfg\(test\)\]/{exit} {print}' "$file")"
    if echo "$production" | grep -E '(TODO|FIXME|XXX|HACK)\b' | grep -v '// ci-ok' | grep -qv '^[[:space:]]*//' 2>/dev/null; then
        echo "FAIL: $file contains TODO/FIXME/HACK"
        echo "$production" | grep -nE '(TODO|FIXME|XXX|HACK)\b' | grep -v '// ci-ok' | grep -v '^[[:space:]]*//'
        FOUND=true
    fi
done < <(find crates -name '*.rs' -not -path '*/tests/*' -not -name '*_test.rs' \
    -not -name '*_tests.rs' -not -name 'tests.rs' -not -path '*/temper-macros/*')
if [ "$FOUND" = true ]; then
    exit 1
fi

FOUND=false
while IFS= read -r file; do
    production="$(awk '/^#\[cfg\(test\)\]/{exit} {print}' "$file")"
    filtered="$(echo "$production" | grep '\.unwrap()' | grep -v '// ci-ok' \
        | grep -v '^[[:space:]]*//' | grep -v '\.read()\.unwrap()' \
        | grep -v '\.write()\.unwrap()' | grep -v '\.lock()\.unwrap()' \
        | grep -v 'with_ymd_and_hms.*\.unwrap()' || true)"
    if [ -n "$filtered" ]; then
        echo "FAIL: $file contains .unwrap()"
        echo "$filtered"
        FOUND=true
    fi
done < <(find crates -name '*.rs' -not -path '*/tests/*' -not -name '*_test.rs' \
    -not -name '*_tests.rs' -not -name 'tests.rs' -not -path '*/temper-macros/*' \
    -not -path '*/benches/*')
if [ "$FOUND" = true ]; then
    exit 1
fi

bash scripts/readability-ratchet.sh check .ci/readability-baseline.env
bash scripts/check-storage-dispatch-boundary.sh

if cargo tree --no-dev -p temper-jit 2>/dev/null | grep -q temper-verify; then
    echo "FAIL: temper-jit has production dependency on temper-verify"
    exit 1
fi
for crate in temper-jit temper-server temper-runtime; do
    if cargo tree --no-dev -p "$crate" 2>/dev/null | grep -qE 'stateright|proptest'; then
        echo "FAIL: $crate production binary includes stateright or proptest"
        exit 1
    fi
done

echo "Integrity and dependency isolation: OK"
