#!/bin/bash
set -euo pipefail
echo "=== DST Coverage Check ==="
echo "Checking that every .ioa.toml has corresponding DST tests..."

WORKSPACE_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
MISSING=0

for spec in $(find "$WORKSPACE_ROOT" -name "*.ioa.toml" -not -path "*/target/*"); do
    ENTITY=$(basename "$spec" .ioa.toml)
    # Look for DST test mentioning this entity
    if ! grep -r "dst\|deterministic_simulation\|sim_" "$WORKSPACE_ROOT/crates" --include="*.rs" -l 2>/dev/null | xargs grep -l "$ENTITY" >/dev/null 2>&1; then
        echo "  MISSING DST: $spec"
        MISSING=$((MISSING + 1))
    fi
done

if [ "$MISSING" -gt 0 ]; then
    echo ""
    echo "$MISSING spec(s) lack DST coverage."
    exit 1
else
    echo "All specs have DST coverage."
fi
