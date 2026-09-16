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
