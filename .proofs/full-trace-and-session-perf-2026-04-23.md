# Full Trace And Session Perf Verification

Date: 2026-04-23
Worktree: `/Users/seshendranalla/Development/temper-worktrees/full-trace-and-session-perf`
Branch: `codex/full-trace-and-session-perf`

## Scope

- Keep LLM spans on the active dispatch trace instead of detaching them.
- Add a local text fast path for internal `GET/PUT /tdata/Files('{id}')/$value` requests so WASM does not pay loopback HTTP for UTF-8 file traffic.
- Add a host-side batch HTTP primitive so WASM modules can execute independent text HTTP requests concurrently without inventing their own loopback orchestration.

## Commands

1. `cargo test -p temper-wasm current_traceparent_header_prefers_active_span_context -- --nocapture`
   Result: passed
2. `cargo test -p temper-server parse_internal_file_value_request_matches_only_value_paths -- --nocapture`
   Result: passed
3. `cargo test -p temper-server llm_root_span_stays_on_active_trace -- --nocapture`
   Result: passed
4. `cargo test -p temper-wasm default_http_call_batch_runs_requests_concurrently -- --nocapture`
   Result: passed
5. `cargo test -p temper-wasm --lib -- --nocapture`
   Result: passed (`71` tests)
6. `cargo run -p temper-cli -- serve --no-observe --port 3313`
   Result: built successfully and reached `Listening on http://0.0.0.0:3313`

## Notes

- Server boot emitted existing ADR-0050 liveness warnings from loaded specs; these predate this change.
- The runtime boot check confirms the tracing changes, local file fast path, and new batch host ABI did not prevent Temper from starting.

## Addendum: 2026-04-24 Query-Plane Stability Verification

### Additional Scope

- Add `query_indexed = false` support to spec parsing and observation output.
- Filter query-plane projections using that spec metadata.
- Add `projection_hash` to `entity_catalog` so unchanged projections update catalog metadata without rebuilding field rows.

### Additional Commands

7. `cargo test -p temper-spec test_state_query_index_flag_parsed -- --nocapture`
   Result: passed
8. `cargo test -p temper-store-turso unchanged_projection_updates_catalog_without_rebuilding_field_rows -- --nocapture`
   Result: passed
9. `cargo test -p temper-server query_projection_excludes_fields_marked_not_query_indexed -- --nocapture`
   Result: passed
10. `cargo test -p temper-store-turso --lib -- --nocapture`
    Result: passed (`29` tests)
11. `cargo test -p temper-server --test query_projection_backfill -- --nocapture`
    Result: passed (`3` tests)

### Additional Notes

- The no-op projection test proves `entity_catalog.sequence_nr` still advances even when the durable field-index rows do not need to be rewritten.
- OpenPaw’s Session E2E on the patched stack confirmed the practical outcome: heartbeat/progress hot fields were excluded from `entity_field_index` while the session still completed and the catalog row advanced.
