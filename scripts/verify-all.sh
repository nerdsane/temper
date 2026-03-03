#!/bin/bash
set -euo pipefail
echo "=== Temper Full Verification Cascade ==="
echo "Running cargo test --workspace..."
cargo test --workspace
echo ""
echo "=== All tests passed ==="
