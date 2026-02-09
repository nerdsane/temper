#!/usr/bin/env bash
# Run all Temper benchmarks.
#
# Usage:
#   ./scripts/bench.sh              # Run all benchmarks
#   ./scripts/bench.sh jit          # TransitionTable micro-benchmarks
#   ./scripts/bench.sh actor        # Server actor dispatch overhead
#   ./scripts/bench.sh ecommerce    # E-commerce agent checkout (realistic)
#
# Set DATABASE_URL to include Postgres benchmarks:
#   DATABASE_URL=postgres://user:pass@localhost/db ./scripts/bench.sh

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

case "${1:-all}" in
  jit)
    echo "=== TransitionTable micro-benchmarks ==="
    cargo bench -p temper-jit --bench table_eval
    ;;
  actor)
    echo "=== Actor dispatch overhead ==="
    cargo bench -p temper-server --bench actor_throughput
    ;;
  ecommerce)
    echo "=== E-commerce agent checkout ==="
    cargo bench -p ecommerce-reference --bench agent_checkout
    ;;
  all)
    echo "=== TransitionTable micro-benchmarks ==="
    cargo bench -p temper-jit --bench table_eval
    echo ""
    echo "=== Actor dispatch overhead ==="
    cargo bench -p temper-server --bench actor_throughput
    echo ""
    echo "=== E-commerce agent checkout ==="
    cargo bench -p ecommerce-reference --bench agent_checkout
    ;;
  *)
    echo "Usage: $0 [jit|actor|ecommerce|all]"
    exit 1
    ;;
esac

echo ""
if [ -z "${DATABASE_URL:-}" ]; then
  echo "Note: Postgres benchmarks skipped (set DATABASE_URL to enable)"
fi
echo "Done. HTML reports in target/criterion/"
