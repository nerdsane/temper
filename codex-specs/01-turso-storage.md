# Codex Spec: Turso Storage Backend

## Goal
Add a `temper-store-turso` crate that implements the `EventStore` trait from `temper-runtime` using Turso/libSQL.

## Context
- `EventStore` trait: `crates/temper-runtime/src/persistence/mod.rs`
- Reference implementation: `crates/temper-store-postgres/src/lib.rs`
- Existing stores: `temper-store-postgres`, `temper-store-redis`

## Requirements

### New crate: `crates/temper-store-turso/`
- Add to workspace `Cargo.toml`
- Dependency: `libsql` crate (https://crates.io/crates/libsql)
- Implement all `EventStore` trait methods:
  - `append(persistence_id, expected_sequence, events)` → optimistic concurrency via sequence check
  - `read_events(persistence_id, from_sequence)` → ordered by sequence_nr
  - `save_snapshot(persistence_id, sequence_nr, snapshot)` → upsert
  - `load_snapshot(persistence_id)` → latest snapshot
  - `list_entity_ids(tenant)` → distinct (entity_type, entity_id) pairs

### Schema
Mirror the Postgres schema but in SQLite-compatible DDL:
```sql
CREATE TABLE IF NOT EXISTS events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    tenant TEXT NOT NULL,
    entity_type TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    sequence_nr INTEGER NOT NULL,
    event_type TEXT NOT NULL,
    payload TEXT NOT NULL,  -- JSON
    metadata TEXT,          -- JSON
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(tenant, entity_type, entity_id, sequence_nr)
);

CREATE INDEX IF NOT EXISTS idx_events_entity ON events(tenant, entity_type, entity_id, sequence_nr);

CREATE TABLE IF NOT EXISTS snapshots (
    tenant TEXT NOT NULL,
    entity_type TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    sequence_nr INTEGER NOT NULL,
    snapshot BLOB NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY(tenant, entity_type, entity_id)
);
```

### Constructor
```rust
pub struct TursoEventStore { /* ... */ }

impl TursoEventStore {
    /// Connect to a Turso database.
    /// url: "libsql://your-db.turso.io" or "file:local.db" for local SQLite
    /// auth_token: Turso auth token (None for local)
    pub async fn new(url: &str, auth_token: Option<&str>) -> Result<Self, ...>

    /// Run schema migrations on connect
    async fn migrate(&self) -> Result<(), ...>
}
```

### Tests
- Mirror the tests in `temper-store-postgres` but using local SQLite file (`file::memory:` or temp file)
- Test: append + read_events roundtrip
- Test: optimistic concurrency (append with wrong sequence fails)
- Test: snapshot save + load
- Test: list_entity_ids returns correct pairs
- Test: schema idempotency (migrate twice = no error)

### Do NOT
- Change any existing crate
- Modify the `EventStore` trait
- Add feature flags to existing crates
