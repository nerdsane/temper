# ADR-0137: Valid Truncated Trajectory Request Bodies

## Status

Accepted

## Context

Temper persists trajectory entries for dispatched entity actions. These entries may
include an HTTP request body snapshot for later debugging and evolution analysis.

The previous request-body helper serialized the JSON body and sliced the resulting
string at 4096 bytes. That preserved a byte cap, but it could cut through an open
JSON string. PostgreSQL then rejected the value when parsing it as `JSONB`, causing
trajectory persistence failures such as:

`EOF while parsing a string at line 1 column 4096`

In TemperPaw production this appeared during `Session.ContextReady`, which is part
of the session/context recovery path.

## Decision

Trajectory request bodies remain bounded to 4096 bytes at the platform storage
adapter boundary.

If the serialized request body fits within the cap, Temper persists the original
JSON unchanged. If it exceeds the cap, Temper persists a valid JSON envelope:

- `_temper_truncated: true`
- `original_bytes`
- `preview_json`

The preview is a UTF-8-safe prefix of the original serialized JSON, and the final
envelope is shrunk until the whole persisted value is under the cap.

## Consequences

Postgres and Turso trajectory sinks now receive parseable JSON for both small and
large request bodies.

Consumers that inspect `request_body` must handle either the original JSON body or
the truncation envelope. This is preferable to losing the trajectory entry entirely.
