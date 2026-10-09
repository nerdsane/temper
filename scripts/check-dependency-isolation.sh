#!/usr/bin/env bash
# Check default-feature, non-dev dependency graphs used by the CI isolation gate.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

for crate in temper-jit temper-server temper-runtime; do
    # Buffer the complete graph and check Cargo's exit before inspecting it.
    # A grep -q pipeline can hide query errors or cause a producer SIGPIPE.
    if graph="$(cargo tree --edges no-dev -p "$crate" --color never)"; then
        if [[ ! "$graph" =~ [^[:space:]] ]]; then
            echo "FAIL: cargo tree returned an empty dependency graph for $crate" >&2
            exit 1
        fi
    else
        status=$?
        echo "FAIL: cargo tree --edges no-dev -p $crate failed (exit $status)" >&2
        exit "$status"
    fi

    if [[ "$crate" == temper-jit && "$graph" =~ (^|[[:space:]])temper-verify[[:space:]] ]]; then
        echo "FAIL: temper-jit has production dependency on temper-verify" >&2
        exit 1
    fi
    if [[ "$graph" =~ (^|[[:space:]])(stateright|proptest)[[:space:]] ]]; then
        echo "FAIL: $crate production binary includes stateright or proptest" >&2
        exit 1
    fi
done

echo "Dependency isolation: OK"
