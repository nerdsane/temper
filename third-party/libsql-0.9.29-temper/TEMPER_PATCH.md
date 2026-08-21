# Temper patch: idempotent local connection teardown

This directory is the published `libsql` 0.9.29 crate source. Temper overrides
the crates.io package through the workspace `[patch.crates-io]` table because
local connections otherwise call `sqlite3_close_v2` twice during teardown.

The sole source change sets the raw SQLite handle to null after the first close,
making the second `disconnect()` call a safe no-op. It matches the fix proposed
in [tursodatabase/libsql#2261](https://github.com/tursodatabase/libsql/pull/2261)
for [tursodatabase/libsql#2251](https://github.com/tursodatabase/libsql/issues/2251).

Remove this override after a released libSQL version contains the upstream fix
and the parallel lifecycle regression continues to pass against that release.
