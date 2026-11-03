# Postgres migrations

Every migration must be additive and backward compatible: the previous release must keep working on a database this
release migrated, because a rollback runs the previous release on that database.

The runner ignores versions it does not know (`migration::migrator`, `set_ignore_missing`), so the previous release
boots. That is only safe when every migration follows these rules:

- Add tables, columns, indexes and policies. Never drop, rename or retype a table or column, and never drop a
  constraint the previous release relies on.
- A new column is nullable or has a default, so the previous release's inserts, which do not name it, still succeed.
- Never truncate or delete rows in a migration.
- Never edit a migration that has been released. Its checksum is recorded, and every release checks it.

A change that cannot follow these rules is two releases: the first adds the new shape and writes both, and a later
one, after the old shape is no longer a rollback target, removes the old shape.

`scripts/check-additive-migrations.sh` (CI, Integrity) refuses a migration line that drops, renames, retypes, adds a
NOT NULL column without a default, truncates or deletes. A line that is deliberately safe carries
`additive-ok: <why>` in a comment on that line or the line above. `0015_ots_trajectory_tenant_identity.sql` predates
the rule and is the only exception.

The Turso store has no migration table: it creates tables and adds columns at startup when they are missing
(`temper-store-turso/src/schema*`), so an older release ignores what it does not know. The same rules and the same
check apply to its schema statements.
