# ADR-0040: Segmented Event History And Bounded Replay

## Status

Accepted

## Context

Temper actors need bounded startup, replay, memory, verification, and recovery. The previous event budget was effectively a lifetime cap: once an entity had recorded 10,000 events, the actor refused new transitions even if a recent snapshot could hydrate it cheaply.

That made long-lived logical entities, especially file-heavy workspaces, eventually fail for a storage-budget reason rather than a domain reason.

## Decision

Keep logical entity history unbounded, but keep each actor's hot replay tail bounded.

- `events.sequence_nr` remains the lifetime sequence and authoritative audit order.
- Event rows carry `segment_index` for storage and audit grouping.
- Stores maintain `event_segments` metadata alongside the latest `snapshots` fast path (immutable `snapshot_history` too, until the 2026-10-26 amendment).
- Snapshot save seals the current segment and opens the next segment.
- Actor state tracks `total_event_count` as lifetime metadata, plus `events_since_snapshot` and `last_snapshot_sequence_nr` as hot-state budget fields.
- The runtime budget is `MAX_EVENTS_SINCE_SNAPSHOT`, defaulting to 10,000.
- Hydration loads the latest snapshot and replays only the post-snapshot tail. It rejects only when that tail exceeds the replay cap.

## Consequences

Long-lived entities can exceed 10,000 lifetime events without becoming invalid. Actors still never need to hydrate or retain an unbounded event stream. Full audit remains available by reading lifetime events and segment metadata.

Existing unsegmented event rows default to segment `0`. No event pruning is part of this decision.

## Amendment (2026-10-26): snapshot history is no longer written

Stores stopped writing `snapshot_history`. Nothing read a stored copy back: its only reader listed tenants. A snapshot is derived from the record's events, which stay whole, so the lifetime events are the full audit. Every copy cost the snapshot's size again, and in Turso's change log twice more. On a record with a full idempotency-key set, that was about 48 KB per snapshot. Existing rows and the table are left in place.
