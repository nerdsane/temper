#!/bin/bash
set -euo pipefail
echo "=== Dependency Audit ==="
WORKSPACE_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

echo "Checking temper-jit does not depend on temper-verify..."
if grep -q 'temper-verify' "$WORKSPACE_ROOT/crates/temper-jit/Cargo.toml"; then
    # Check it's only in dev-dependencies
    if cargo metadata --manifest-path "$WORKSPACE_ROOT/Cargo.toml" --format-version 1 2>/dev/null | \
        python3 -c "
import json, sys
data = json.load(sys.stdin)
for pkg in data['packages']:
    if pkg['name'] == 'temper-jit':
        for dep in pkg['dependencies']:
            if dep['name'] == 'temper-verify' and dep.get('kind') != 'dev':
                print('FAIL: temper-jit has non-dev dependency on temper-verify')
                sys.exit(1)
print('OK: temper-jit only has dev-dependency on temper-verify (if any)')
"; then
        :
    else
        echo "FAIL: temper-jit depends on temper-verify in production!"
        exit 1
    fi
else
    echo "OK: temper-jit does not reference temper-verify at all."
fi

echo ""
echo "Checking production binary does not pull in stateright..."
if cargo tree -p temper-server --no-dev 2>/dev/null | grep -q stateright; then
    echo "FAIL: temper-server production binary includes stateright!"
    exit 1
else
    echo "OK: stateright not in temper-server production deps."
fi

echo ""
echo "=== Dependency audit passed ==="
