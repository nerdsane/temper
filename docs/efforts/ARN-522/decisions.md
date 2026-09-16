# Decisions and tradeoffs

## D1: Prefer the actor when it is loaded, instead of making the catalog write synchronous

**Decision:** A read consults the catalog only for entities whose actor is
not in memory.

**Came up because:** The merge read-back returned the pre-merge PullRequest
row. Tracing the request showed the rebuilt actor already held `Merged` at
the moment the read answered `Approved`: the read came from the catalog, whose
row is written by the background projection queue after the dispatch returns.

**Options:** Await the queued catalog write before a dispatch returns; write
the catalog synchronously on the composite path only; let a loaded actor win
over the catalog.

**Chose the loaded actor because** it restores read-your-writes for every
dispatch path, not only composites, without putting the catalog write on the
dispatch's critical path — which is the reason the queue exists. An entity
that was just dispatched is loaded by construction, and asking a loaded actor
is a local message, not a database read.

**Where.** `crates/temper-server/src/odata/read_support.rs`;
`crates/temper-server/src/state/entity_ops.rs` (`has_loaded_actor`).

## D2: A read never writes the catalog back for an entity whose actor is live

**Decision:** The entity-set fallback still repairs a catalog *miss* from the
actor — including a loaded actor whose row never landed — but when a row
existed and was skipped because the actor is loaded, the read does not upsert
the projection.

**Came up because:** Review round 1 (fable, act-on): with D1 alone, every hot
row on a collection page went through the actor fallback, which upserts the
projection unconditionally — one catalog write per hot entity per read, and,
since the upsert carries the sequence it read, a read racing a newer queued
write could put an older row back.

**Options:** Keep the unconditional repair; make the repair sequence-guarded
in SQL; skip the repair when the actor was preferred.

**Chose skipping because** the repair exists for rows the catalog lacks, and
for a live entity the projection queue already owns the row and carries the
newest sequence. A sequence guard in the store is a fine second line of
defence but is not what this effort needs.

**Where.** `crates/temper-server/src/odata/read_support.rs`,
`materialize_entity_set_entities`, `read_support/projection_repair.rs`; the
test asserts the catalog row is untouched after a collection read with the
actor loaded. Review round 2 (codex) caught the first cut skipping the repair
for every loaded actor, which would have left a queue-dropped row absent for
good; the skip is now keyed on "a row was present and skipped".

## D3: The regression test runs against a local Turso store, not a seeded simulation

**Decision:** The test seeds a stale catalog row in a per-process Turso file
database, spawns the actor, and reads through both the single-entity helper
and the entity-set materialization.

**Came up because:** Review round 1 (codex, act-on; grok) asked for a seeded
simulator scenario with a read-after-acknowledgement invariant instead.

**Options:** A DST scenario that delays projection delivery under a seed; the
Turso-backed test the repository's other OData/observe tests already use.

**Chose the Turso-backed test because** the rule under test — "a loaded actor
wins over the catalog" — is a read-path decision with no timing in it; the
timing (queue delay) is what the rule makes irrelevant, so a delayed-delivery
seed would exercise the queue, not the rule. The test is the same shape as
`observe/mod_test.rs`. A read-after-acknowledgement invariant for the
simulator is worth having and is recorded on ARN-522 as a follow-up, not a
prerequisite.

**Where.** `crates/temper-server/src/odata/read_support/tests.rs`.
