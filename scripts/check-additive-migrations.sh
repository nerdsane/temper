#!/usr/bin/env bash
# Refuse schema statements a rollback cannot survive (crates/temper-store-postgres/migrations/README.md):
# dropping, renaming or retyping, a NOT NULL column without a default, truncating or deleting rows. Scans the
# Postgres migrations and the Turso store's startup schema. A deliberately safe line carries "additive-ok: <why>"
# in a comment on that line or the line above.
set -euo pipefail
cd "$(dirname "$0")/.."

# Written before the rule; each is a release that is no longer a rollback target.
grandfathered=(
  "crates/temper-store-postgres/migrations/0015_ots_trajectory_tenant_identity.sql:*"
  "crates/temper-store-turso/src/schema/ots.rs:pub const DROP_OTS_TRAJECTORIES_LEGACY_TABLE: &str = \"DROP TABLE ots_trajectories;\";"
  "crates/temper-store-turso/src/schema/ots.rs:ALTER TABLE ots_trajectories_rebuild RENAME TO ots_trajectories;\";"
)

destructive='\b(DROP[[:space:]]+(TABLE|COLUMN|CONSTRAINT|TYPE|SCHEMA|VIEW)|RENAME|TRUNCATE|DELETE[[:space:]]+FROM|SET[[:space:]]+NOT[[:space:]]+NULL|ALTER[[:space:]]+COLUMN.*[[:space:]]TYPE)\b'

files=(crates/temper-store-postgres/migrations/*.sql crates/temper-store-turso/src/schema.rs
  crates/temper-store-turso/src/schema_*.rs crates/temper-store-turso/src/schema/*.rs)

found=0
for file in "${files[@]}"; do
  case "$file" in *_test.rs) continue ;; esac
  previous=""
  while IFS= read -r line || [ -n "$line" ]; do
    trimmed="${line#"${line%%[![:space:]]*}"}"
    flagged=""
    case "$trimmed" in --* | //*) ;; *)
      if grep -qiE "$destructive" <<<"$trimmed"; then flagged=1; fi
      if grep -qiE 'ADD[[:space:]]+COLUMN.*NOT[[:space:]]+NULL' <<<"$trimmed" && ! grep -qi 'DEFAULT' <<<"$trimmed"; then
        flagged=1
      fi
      ;;
    esac
    if [ -n "$flagged" ] && ! grep -q 'additive-ok:' <<<"$previous$line"; then
      for entry in "${grandfathered[@]}"; do
        if [ "$entry" = "$file:*" ] || [ "$entry" = "$file:$trimmed" ]; then flagged=""; fi
      done
    fi
    if [ -n "$flagged" ] && ! grep -q 'additive-ok:' <<<"$previous$line"; then
      echo "$file: not additive: $trimmed"
      found=1
    fi
    previous="$line"
  done <"$file"
done

if [ "$found" -ne 0 ]; then
  echo "A rollback runs the previous release on this schema; see crates/temper-store-postgres/migrations/README.md." >&2
  exit 1
fi
echo "Additive migrations: OK"
