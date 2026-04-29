#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MAX_VIOLATIONS="${MAX_STORAGE_BOUNDARY_VIOLATIONS:-39}"
STRICT="${TEMPER_STORAGE_DISPATCH_STRICT:-0}"

PATTERN='ServerEventStore|\.event_store\b|event_store\.as_ref|collect_all_turso_stores|persistent_store_for_tenant|platform_persistent_store|metadata_backend_for_tenant'

VIOLATIONS="$(
  cd "$ROOT"
  rg -n "$PATTERN" crates/temper-server/src crates/temper-cli/src \
    -g '!crates/temper-server/src/event_store.rs' \
    -g '!crates/temper-server/src/storage/mod.rs' \
    -g '!*test.rs' \
    -g '!*/tests/*' \
    || true
)
"

if [[ -z "$VIOLATIONS" ]]; then
  COUNT=0
else
  COUNT="$(printf '%s\n' "$VIOLATIONS" | wc -l | tr -d '[:space:]')"
fi

if [[ "$STRICT" == "1" && "$COUNT" -gt 0 ]]; then
  printf 'FAIL: storage dispatch boundary has %s production violations in strict mode.\n' "$COUNT" >&2
  printf '%s\n' "$VIOLATIONS" >&2
  exit 1
fi

if (( COUNT > MAX_VIOLATIONS )); then
  printf 'FAIL: storage dispatch boundary violations increased: %s > %s.\n' "$COUNT" "$MAX_VIOLATIONS" >&2
  printf 'Move new code onto StorageStack capability traits or reduce MAX_STORAGE_BOUNDARY_VIOLATIONS only after deleting legacy use.\n' >&2
  printf '%s\n' "$VIOLATIONS" >&2
  exit 1
fi

printf 'Storage dispatch boundary: OK (%s/%s legacy violations).\n' "$COUNT" "$MAX_VIOLATIONS"
