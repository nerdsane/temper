#!/bin/bash
# Verify every IOA directory that has a companion CSDL model.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SPEC_DIRS="$(find "$ROOT" -name '*.ioa.toml' -not -path '*/target/*' -exec dirname {} \; | sort -u)"

if [ -z "$SPEC_DIRS" ]; then
    echo "No spec directories found — skipping verification"
    exit 0
fi

while IFS= read -r directory; do
    if [ -f "$directory/model.csdl.xml" ]; then
        echo "Verifying specs in ${directory#"$ROOT"/}..."
        cargo run --manifest-path "$ROOT/Cargo.toml" -p temper-cli -- verify --specs-dir "$directory"
    fi
done <<< "$SPEC_DIRS"

echo "All specs verified: OK"
