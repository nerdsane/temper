# Codex Spec: Storage Backend CLI Selection

## Goal
Let users choose storage backend at startup via CLI flag.

## Context
- CLI entry: `crates/temper-cli/src/main.rs`
- Current: hardcodes `PostgresEventStore` when `DATABASE_URL` is set
- New crate from spec 01: `temper-store-turso`
- Existing: `temper-store-postgres`, `temper-store-redis`

## Requirements

### CLI changes
Add `--storage` flag to the `serve` subcommand:

```
temper serve --storage postgres --app my-app=path/to/specs --port 3001
temper serve --storage turso --app my-app=path/to/specs --port 3001
temper serve --storage redis --app my-app=path/to/specs --port 3001
```

Default: `postgres` (backwards compatible).

### Environment variables per backend
- `postgres`: `DATABASE_URL` (existing)
- `turso`: `TURSO_URL` + optional `TURSO_AUTH_TOKEN`
- `redis`: `REDIS_URL` (existing)

### Startup logic
```rust
let store: Arc<dyn EventStore> = match storage_flag {
    "postgres" => Arc::new(PostgresEventStore::new(&database_url).await?),
    "turso" => Arc::new(TursoEventStore::new(&turso_url, turso_token.as_deref()).await?),
    "redis" => Arc::new(RedisEventStore::new(&redis_url).await?),
    _ => return Err("Unknown storage backend"),
};
```

### Validation
- Error early if required env var is missing for chosen backend
- Print storage backend in startup log: `Storage: postgres (postgres://...@localhost/haku_ops)`

### Do NOT
- Change the `EventStore` trait
- Remove `DATABASE_URL` fallback behavior (if no `--storage` flag and `DATABASE_URL` is set, default to postgres)
