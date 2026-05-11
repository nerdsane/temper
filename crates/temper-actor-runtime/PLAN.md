# Generic Actor Runtime — Implementation Plan

## What we're building

A PG-backed actor runtime for Temper. Pure Layer 1 — no specs, no subscriptions,
no event type routing. Actors communicate via addressed messages (mailboxes),
not pub/sub on a shared event log.

This is the foundation. Spec-driven behavior (IOA, TransitionTable) is Layer 2,
built on top of this later.

---

## Core concepts

| Concept | Description |
|---|---|
| **ActorHandle** | An address: `(session_id, actor_type)`. Immutable, copyable, serializable. |
| **Message** | Addressed envelope: from, to, type, payload (bytes), optional correlation_id. |
| **Actor** | A trait: receives messages one at a time, updates state, sends messages. |
| **ActorContext** | Passed to handle() — provides self_handle, spawn, lookup, tell, ask. |
| **Mailbox** | Per-actor message queue in PG. Messages addressed TO this actor. |
| **tell()** | Fire-and-forget: buffered in context, committed transactionally on success. |
| **ask()** | Request-response: immediate I/O during handle(), blocks until response. |
| **Scheduler** | Polls mailboxes, activates actors (one at a time), persists state. |

---

## PG schema

### `odp_temper.actor_messages` — the mailbox table

All messages between actors. Each message is addressed to a specific actor.
This is also the event log — the full history of all communication.

Payload is `BYTEA` — the runtime is format-agnostic. Actors can use protobuf,
JSON, MessagePack, or any serialization format. The runtime doesn't inspect
the payload; it just delivers bytes.

Trade-off: we lose PG-level queryability on payloads (no `payload->>'field'`).
For debugging, actors can register a deserializer that the tooling uses to
pretty-print messages. Production observability comes from tracing, not PG queries.

```sql
CREATE TABLE odp_temper.actor_messages (
    id              BIGSERIAL PRIMARY KEY,
    session_id      UUID NOT NULL,
    to_actor        TEXT NOT NULL,
    from_actor      TEXT,
    message_type    TEXT NOT NULL,
    payload         BYTEA NOT NULL,
    correlation_id  UUID,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_actor_messages_mailbox
    ON odp_temper.actor_messages (session_id, to_actor, id);

CREATE INDEX idx_actor_messages_correlation
    ON odp_temper.actor_messages (correlation_id) WHERE correlation_id IS NOT NULL;
```

### `odp_temper.actor_types` — registered actor type definitions

Actor type registrations are persisted in PG. Any pod can activate any
actor — state is in PG and all pods run the same handler code. The table
is used by `spawn()` to validate the actor type exists before creating
an instance.

```sql
CREATE TABLE odp_temper.actor_types (
    actor_type    TEXT PRIMARY KEY,
    registered_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
```

### `odp_temper.actor_instances` — actor state + cursor

State is also `BYTEA` — actors own their serialization format.

```sql
CREATE TABLE odp_temper.actor_instances (
    session_id    UUID NOT NULL,
    actor_type    TEXT NOT NULL,
    state         BYTEA NOT NULL DEFAULT '',
    last_msg_id   BIGINT NOT NULL DEFAULT 0,
    version       BIGINT NOT NULL DEFAULT 0,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (session_id, actor_type)
);
```

---

## Rust traits

### ActorHandle — an address

Named "Handle" not "Ref" to avoid confusion with Rust's `&T` references.

```rust
#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct ActorHandle {
    pub session_id: Uuid,
    pub actor_type: String,
}
```

### Message — what actors receive

```rust
pub struct Message {
    pub id: i64,
    pub from: Option<ActorHandle>,
    pub to: ActorHandle,
    pub message_type: String,
    pub payload: Vec<u8>,
    pub correlation_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}
```

### ActorContext — the actor's interface to the system

Passed to `handle()`. Provides everything the actor needs to interact
with the outside world: identity, spawning, messaging.

```rust
pub struct ActorContext {
    self_handle: ActorHandle,
    mailbox: Arc<dyn Mailbox>,
    // internal buffer for tell() messages — committed on tx success
    pending_tells: RefCell<Vec<OutMessage>>,
}

impl ActorContext {
    /// This actor's own address.
    pub fn self_handle(&self) -> &ActorHandle;

    /// Spawn a new actor instance in the same session.
    pub async fn spawn(&self, actor_type: &str) -> Result<ActorHandle, ActorError>;

    /// Look up a sibling actor in the same session.
    pub async fn lookup(&self, actor_type: &str) -> Option<ActorHandle>;

    /// Fire-and-forget message. Buffered internally — only committed
    /// to PG when the activation transaction succeeds. If handle()
    /// fails, buffered tells are discarded (not sent).
    pub fn tell(
        &self,
        to: &ActorHandle,
        message_type: &str,
        payload: Vec<u8>,
    );

    /// Request-response. Sends a message and blocks until a reply
    /// arrives (or timeout). This is IMMEDIATE I/O — the message is
    /// sent and the response is awaited during handle(). Unlike tell(),
    /// ask() is NOT transactional: if handle() fails after ask(),
    /// the ask message was already sent and the response was already
    /// received. The caller must handle this.
    ///
    /// Use ask() sparingly. Prefer tell() + callback pattern for
    /// most actor-to-actor communication.
    pub async fn ask(
        &self,
        to: &ActorHandle,
        message_type: &str,
        payload: Vec<u8>,
        timeout: Duration,
    ) -> Result<Message, ActorError>;
}
```

### Actor trait

```rust
#[async_trait]
pub trait Actor: Send + Sync + 'static {
    /// Unique type name for this actor (e.g., "Session", "ToolRunner").
    fn actor_type(&self) -> &str;

    /// Initial state for a new actor instance (serialized bytes).
    fn initial_state(&self) -> Vec<u8> {
        vec![]
    }

    /// Handle a single message.
    ///
    /// Use ctx.tell() to send fire-and-forget messages (buffered, transactional).
    /// Use ctx.ask() to send request-response messages (immediate, NOT transactional).
    ///
    /// State is &mut Vec<u8> — the actor owns serialization/deserialization.
    /// The runtime persists the state bytes after handle returns Ok(()).
    ///
    /// One message at a time (classic actor model). The runtime calls
    /// handle() for each message in mailbox order (FIFO).
    ///
    /// **Important**: handle() must be idempotent or side-effect-free.
    /// On transient failures, the runtime may retry the entire activation.
    async fn handle(
        &self,
        ctx: &ActorContext,
        state: &mut Vec<u8>,
        message: &Message,
    ) -> Result<(), ActorError>;
}
```

### Mailbox — the low-level send/ask interface

Used internally by ActorContext and by external callers (API layer).

```rust
#[async_trait]
pub trait Mailbox: Send + Sync + 'static {
    /// Fire-and-forget: deliver a message to an actor's mailbox.
    async fn tell(
        &self,
        from: Option<&ActorHandle>,
        to: &ActorHandle,
        message_type: &str,
        payload: Vec<u8>,
    ) -> Result<i64, MailboxError>;

    /// Request-response: send and wait for reply.
    /// Poll interval: configurable, default 10ms.
    /// Timeout: required parameter.
    async fn ask(
        &self,
        from: &ActorHandle,
        to: &ActorHandle,
        message_type: &str,
        payload: Vec<u8>,
        timeout: Duration,
    ) -> Result<Message, MailboxError>;
}
```

### ActorActivator — atomic activation

Single PG transaction per activation.

```rust
#[async_trait]
pub trait ActorActivator: Send + Sync + 'static {
    /// Activate an actor: lock → load state → read next message →
    /// call handler → persist state → flush buffered tells → commit.
    async fn activate(
        &self,
        actor_handle: &ActorHandle,
        handler: &dyn Actor,
    ) -> Result<ActivationResult, ActivationError>;
}
```

---

## Scheduler

Polls for actors with pending messages. Simple loop:

```
loop {
    pending = find_actors_with_messages()
    for actor in pending {
        activator.activate(actor, handler)
    }
    if nothing processed: sleep(poll_interval)
}
```

### Finding pending actors

```sql
SELECT DISTINCT ai.session_id, ai.actor_type
FROM odp_temper.actor_instances ai
WHERE EXISTS (
    SELECT 1 FROM odp_temper.actor_messages am
    WHERE am.session_id = ai.session_id
      AND am.to_actor = ai.actor_type
      AND am.id > ai.last_msg_id
)
ORDER BY random()
LIMIT $1
```

No subscription resolution. No inverted index. Just: "does this actor have
unread messages?" Pure mailbox polling.

### Locking for single-pod scheduling

The `find_pending` query only finds CANDIDATES. The actual single-writer
guarantee is the advisory lock inside `activate()`:

```
pg_try_advisory_xact_lock(hashtext(session_id), hashtext(actor_type))
```

Two-key, 64-bit, transaction-scoped. If Pod A is already activating an
actor, Pod B sees the lock, returns `activated: false`, moves on.

`ORDER BY random()` spreads candidates across pods.

### Activation (single PG transaction)

1. BEGIN
2. pg_try_advisory_xact_lock(hashtext(session_id), hashtext(actor_type))
3. Load actor state + cursor
4. Read NEXT message from mailbox (ONE message, FIFO order)
5. Build ActorContext (with empty tell buffer)
6. Call handler.handle(ctx, state, message)
7. If Ok(): persist state + advance cursor + flush ctx.pending_tells
8. If Err(): ROLLBACK (buffered tells discarded, state unchanged)
9. COMMIT (lock auto-releases)

Note on ask() during step 6: ask() sends a message IMMEDIATELY (outside
the transaction) and blocks for the response. If handle() fails after
a successful ask(), the ask side-effects are NOT rolled back. This is
by design — ask() is inherently non-transactional. Actors should prefer
the tell()+callback pattern for transactional safety.

---

## ActorSystem — the top-level API

Convenience wrapper that ties everything together. Uses interior mutability
(`RwLock`) so the system can be shared across tasks.

```rust
pub struct ActorSystem {
    mailbox: Arc<dyn Mailbox>,
    activator: Arc<dyn ActorActivator>,
    scheduler: Scheduler,
    handlers: RwLock<HashMap<String, Box<dyn Actor>>>,  // actor_type → handler (in-memory, same on all pods)
}

impl ActorSystem {
    /// Register an actor implementation. Persists the actor type to PG
    /// (so other pods can discover it) and stores the handler locally.
    async fn register(&self, actor: Box<dyn Actor>);

    /// Spawn an actor instance for a session.
    async fn spawn(&self, session_id: Uuid, actor_type: &str) -> ActorHandle;

    /// Send a message from outside the actor system (e.g., API layer).
    async fn tell(&self, from: Option<&ActorHandle>, to: &ActorHandle, msg_type: &str, payload: Vec<u8>);

    /// Send a message and wait for response (from outside the actor system).
    async fn ask(&self, from: &ActorHandle, to: &ActorHandle, msg_type: &str, payload: Vec<u8>, timeout: Duration) -> Message;

    /// Run the scheduler loop.
    async fn run(&self, cancel: watch::Receiver<bool>);
}
```

---

## What's NOT in this plan (Layer 2, later)

- IOA specs / TransitionTable
- SpecDrivenActor
- Subscription-based routing
- Integration dispatcher
- Watchdog / per-state timeouts
- Actor supervision hierarchies (parent-child restart policies)

### Note on supervision

Actor hierarchies (Erlang-style supervision trees) mean: if a child actor
crashes, the parent gets notified and can decide what to do (restart child,
escalate, stop all children). This requires tracking parent-child
relationships and failure propagation.

Not in v1. For now, crashes just fail the activation and the scheduler
retries on the next poll. The `ctx.spawn()` call sets up the relationship
metadata for later use.

All of the above builds ON TOP of this runtime.

---

## Crate structure

```
temper-actor-runtime/
  src/
    lib.rs
    actor.rs        -- ActorHandle, Message, Actor trait, ActorContext
    mailbox.rs      -- Mailbox trait, PgMailbox implementation
    activator.rs    -- ActorActivator trait, PgActorActivator
    scheduler.rs    -- Scheduler loop
    system.rs       -- ActorSystem convenience wrapper
    schema.rs       -- DDL for local dev/tests (production migrations in k8s-resources)
  tests/
    unit.rs         -- mock tests
    integration.rs  -- real PG tests
```

---

## Implementation order

1. Types (actor.rs): ActorHandle, Message, Actor trait, ActorContext
2. Schema (schema.rs): DDL + query constants
3. PG mailbox (mailbox.rs): tell() and ask()
4. PG activator (activator.rs): single-transaction activation with tell buffer flush
5. Scheduler (scheduler.rs): poll + activate loop
6. System (system.rs): ties it together
7. Tests:
   - Unit with mocks: simple Foo/Bar/Baz actors that send messages to each other
   - Integration with PG: Ping-Pong actors where Ping spawns Pong, sends 3
     messages, Pong replies each time, both track message count in state.
     Verifies: spawn, tell, state persistence, FIFO ordering, message counting.

---

## Open questions (resolved)

1. **One message or batch?** → One. Classic actor model.

2. **ask() polling interval?** → Configurable, default 10ms. Timeout also configurable, required param.

3. **Supervision hierarchies?** → Not in v1. Crashes retry on next poll. ctx.spawn() records parent-child for future use.

4. **Message ordering?** → FIFO within a mailbox (BIGSERIAL + ORDER BY id). Cross-actor ordering not guaranteed. Messages must be idempotent.

5. **Dead letters?** → TODO for now. Messages to nonexistent actors are silently dropped. Add dead-letter table later.

---

## Design decisions log

- **ActorHandle not ActorRef**: "Ref" has specific meaning in Rust (`&T`).
- **Payload as BYTEA not JSONB**: runtime is format-agnostic. Actors choose their serialization (protobuf, JSON, etc.). Lose PG queryability but gain flexibility. Debug via tracing, not SQL.
- **State as BYTEA not JSONB**: same reasoning. Actors own their state format.
- **tell() and ask() on ActorContext**: actors interact with the system through the context, not by returning outbound messages. tell() is buffered and transactional (committed only on success). ask() is immediate and non-transactional (inherent to request-response). This gives actors full control over when and how they communicate.
- **handle() returns Result<(), ActorError>**: not Result<Vec<OutMessage>>. Outbound messages are managed by the context, not the return value. This lets actors use ask() inline and get the response.
- **register() is async and persists to PG**: actor types are stored in `actor_types` table. Any pod can activate any actor — state is in PG, handler code is the same binary on all pods. No local registry needed for scheduling. The `register()` call persists the type so `spawn()` can validate it exists, and maps the type name to its handler implementation in-memory.

- **Interior mutability on ActorSystem**: `RwLock<HashMap>` for registry so register() takes `&self`.

### Transactional semantics

| Operation | During handle() | On Ok() | On Err() |
|---|---|---|---|
| State mutation | In-memory `&mut Vec<u8>` | Persisted to PG | Discarded |
| ctx.tell() | Buffered in context | Flushed to PG (messages inserted) | Discarded |
| ctx.ask() | Sent immediately, response received | Already committed | Already committed (NOT rolled back) |
| ctx.spawn() | Actor instance created in PG | Already committed | TODO: cleanup? |

ask() and spawn() are non-transactional because they involve I/O that
can't be rolled back. Actors should prefer tell()+callback when
transactional safety matters. ask() is a convenience for simple
request-response patterns where the non-transactional risk is acceptable.
