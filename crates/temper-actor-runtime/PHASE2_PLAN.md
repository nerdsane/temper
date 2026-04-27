# Phase 2: Actors on the Mailbox Runtime — Implementation Plan

## Goals

1. **Generate actors from specs** — Load IOA TOML specs, create SpecDrivenActors,
   register with the actor system. Action names ARE the message_type strings.
   Proto types are hand-written (prost derive) for now, codegen from specs later.

2. **CRUD via OData** — Wire into existing Temper OData layer.
   - `POST /tdata/Agent` → creates actor instance via `system.spawn()`
   - `GET /tdata/Agent('id')` → reads actor state
   - `POST /tdata/Agent('id')/StartProcess` → dispatches action via `system.tell()`
   - Same HTTP surface as today, backed by the mailbox runtime instead of the
     old entity actor system.

3. **Actor communication + state validation** — Reaction rules wire actors.
   TransitionTable validates transitions. Invalid messages rejected by the spec.
   Full chain: Agent → ContextManager → ToolRouter → Compactor, all driven by
   specs with state transitions validated at every step.

---

## Key design decisions (from discussion)

### 1. Specs stay pure (Option C)

Specs describe single-actor state machines. They emit messages but don't know
WHO receives them. The routing is external — handled by reaction rules.

```toml
# agent.ioa.toml — spec doesn't know about ContextManager
effect = [{ type = "emit", event = "PrepareContext" }]
```

### 2. Routing via reaction rules (same as current Temper)

Reaction rules are external TOML config that wire actors together:

```toml
[[reaction]]
name = "agent_requests_context"
when = { entity_type = "Agent", action = "StartProcess" }
then = { entity_type = "ContextManager", action = "PrepareContext" }
resolve_target = { type = "SameId" }

[[reaction]]
name = "context_ready_to_agent"
when = { entity_type = "ContextManager", action = "ContextReady" }
then = { entity_type = "Agent", action = "ContextReady" }
resolve_target = { type = "SameId" }

[[reaction]]
name = "agent_requests_tools"
when = { entity_type = "Agent", action = "InferenceCompleteToolCalls" }
then = { entity_type = "ToolRouter", action = "ToolCallBatchRequested" }
resolve_target = { type = "SameId" }

[[reaction]]
name = "tools_complete_to_agent"
when = { entity_type = "ToolRouter", action = "ToolCallBatchComplete" }
then = { entity_type = "Agent", action = "ToolCallBatchComplete" }
resolve_target = { type = "SameId" }

[[reaction]]
name = "agent_requests_compaction"
when = { entity_type = "Agent", action = "ContextOverflow" }
then = { entity_type = "Compactor", action = "CompactionNeeded" }
resolve_target = { type = "SameId" }

[[reaction]]
name = "compaction_complete_to_agent"
when = { entity_type = "Compactor", action = "CompactionComplete" }
then = { entity_type = "Agent", action = "CompactionComplete" }
resolve_target = { type = "SameId" }
```

### 3. Routing resolved at registration time

When actors are registered, the system reads the reaction rules and builds
a per-actor routing map:

```
Agent routing map:
  "PrepareContext"           → tell ContextManager
  "ToolCallBatchRequested"   → tell ToolRouter
  "CompactionNeeded"         → tell Compactor

ContextManager routing map:
  "ContextReady"             → tell Agent
  "ContextOverflow"          → tell Agent

ToolRouter routing map:
  "ToolCallBatchComplete"    → tell Agent
  "ToolCallBatchFailed"      → tell Agent
  "ApprovalRequired"         → tell Agent

Compactor routing map:
  "CompactionComplete"       → tell Agent
  "CompactionFailed"         → tell Agent
```

The routing map is `HashMap<String, String>` — emit name → target actor type.
At runtime, the full ActorHandle is built using the current namespace:
`ActorHandle::new(ctx.self_handle().namespace, target_actor_type)`.

### 4. Namespace embeds tenant

No separate tenant column. The namespace string carries the org context:

```
namespace = "{org_id}/session/{session_id}"
```

All actors in a session share the same namespace.
Cross-namespace routing deferred to later (not Phase 2).

### 5. Actor instances created via OData

OData `POST /tdata/Agent` creates actor instance → calls `system.spawn()`
under the hood. Same HTTP surface as existing Temper. The OData layer
translates HTTP requests into actor system calls.

### 6. Action → message type mapping by convention

IOA action name = message_type string. No explicit mapping needed.
Action "StartProcess" → message with `message_type = "StartProcess"`.
The SpecDrivenActor matches `message.message_type` against the TransitionTable.

### 7. Integration actors use tell() not ask()

tell() + callback is non-blocking and maps cleanly to the state machine:
actor transitions to Blocked state, integration does work, sends callback
via tell(), actor receives callback and transitions. No PG transaction held
open during integration work.

### 8. Proto types generated from specs (codegen)

The IOA spec is the single source of truth for message types. Codegen reads
the spec and generates prost derive structs for each action.

Spec extension — params become typed:

```toml
# Before (untyped):
params = ["user_prompt"]

# After (typed):
[[action]]
name = "StartProcess"
params = [
    { name = "user_prompt", type = "string" },
]

[[action]]
name = "ContextReady"
params = [
    { name = "token_count", type = "uint64" },
    { name = "message_count", type = "uint64" },
]
```

Codegen output:

```rust
#[derive(Clone, PartialEq, prost::Message)]
pub struct StartProcess {
    #[prost(string, tag = "1")]
    pub user_prompt: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ContextReady {
    #[prost(uint64, tag = "1")]
    pub token_count: u64,
    #[prost(uint64, tag = "2")]
    pub message_count: u64,
}
```

Rules:
- Tag numbers auto-assigned (1, 2, 3...)
- Supported types: `string`, `uint64`, `int64`, `bool`, `bytes`, `float`, `double`
- Backward compatible: plain `params = ["user_prompt"]` defaults to `string` type
- Actions with no params generate empty structs: `pub struct PrepareContext {}`

Implementation:
- Extend `temper-spec` param parsing to support typed params
- New codegen tool/build.rs: reads parsed Automaton → generates Rust file with prost structs
- Generated code lives alongside the spec (or in a generated crate)
- Both actors and callers use the generated types

---

## What to build

### 1. Reaction types in temper-runtime

Move the core reaction types into `temper-runtime` (the base crate both
temper-server and temper-actor-runtime depend on):

- `ReactionRule` — when/then/resolve_target
- `ReactionTrigger` — entity_type + action + optional to_state
- `ReactionTarget` — entity_type + action + params
- `TargetResolver` — SameId only for Phase 2 (Field, Static, CreateIfMissing later)
- `ReactionRegistry` — indexed by "EntityType:Action"
- TOML parsing for reaction rules

### 2. Routing map builder

Takes the reaction registry + list of registered actor types → builds
per-actor routing maps. Called once at registration time.

```rust
fn build_routing_maps(
    registry: &ReactionRegistry,
    actor_types: &[String],
) -> HashMap<String, HashMap<String, String>>
// actor_type → (emit_name → target_actor_type)
```

### 3. SpecDrivenActor for the mailbox model

Implements `Actor` trait from temper-actor-runtime:

```rust
struct SpecDrivenActor {
    name: String,
    table: TransitionTable,
    routing: HashMap<String, String>,  // emit name → target actor type
}

impl Actor for SpecDrivenActor {
    async fn handle(&self, ctx: &ActorContext, state: &mut Vec<u8>, message: &Message) {
        // 1. Deserialize state (counters, booleans, status)
        // 2. Build eval context from state
        // 3. Evaluate transition table for message.message_type
        // 4. If valid: apply effects (state changes)
        // 5. For each emit effect: lookup routing → ctx.tell(target, msg)
        // 6. For each trigger effect: ctx.tell(integration_actor, msg)
        // 7. Serialize state back
        // 8. If invalid: log warning, skip (state unchanged)
    }
}
```

### 4. Extend temper-spec for typed params

Update the IOA parser to support typed params:

```toml
# New format (typed):
params = [
    { name = "user_prompt", type = "string" },
    { name = "token_count", type = "uint64" },
]

# Old format still works (defaults to string):
params = ["user_prompt"]
```

Changes to temper-spec:
- `Action.params`: `Vec<String>` → `Vec<ActionParam>`
- `ActionParam`: `{ name: String, param_type: String }`
- Backward-compatible parsing: plain string → `ActionParam { name, param_type: "string" }`

### 5. Proto codegen from specs

New codegen tool that reads a parsed Automaton and generates a Rust file
with prost derive structs for each action:

```rust
// Input: agent.ioa.toml
// Output: agent_messages.rs (generated)

pub struct StartProcess {
    #[prost(string, tag = "1")]
    pub user_prompt: String,
}

pub struct PrepareContext {}

pub struct ContextReady {
    #[prost(uint64, tag = "1")]
    pub token_count: u64,
}
// ... one struct per action
```

Library crate `temper-codegen` called from `build.rs`. Standard Rust
build script approach — generates into `OUT_DIR`, included via `include!`:

```rust
// build.rs
fn main() {
    temper_codegen::generate_messages(
        &["specs/agent.ioa.toml", "specs/context_manager.ioa.toml"],
    ).unwrap();
}

// lib.rs or messages.rs
include!(concat!(env!("OUT_DIR"), "/agent_messages.rs"));
include!(concat!(env!("OUT_DIR"), "/context_manager_messages.rs"));
```

Bazel integration (write_source_files, dd_cargo_build_script) comes later.

### 5. Mock integration actors

Stub actors for LLM, tool executor, compaction. Real implementations in Phase 3.

```rust
struct MockLlmActor;  // receives InvokeModel, replies with InferenceComplete*
struct MockToolActor;  // receives ToolCallBatchRequested, replies with ToolCallBatchComplete
struct MockCompactor;  // receives CompactionNeeded, replies with CompactionComplete
```

### 6. OData wiring

Adapt the existing Temper OData handler to use the actor system:
- Entity creation → `system.spawn(namespace, actor_type)`
- Action dispatch → `system.tell(from, to, ActionMsg { params })`
- State read → `system.load_state(namespace, actor_type)` (need to add this to ActorSystem)
- Entity deletion → TBD

### 7. Reaction rules TOML config

The wiring config for the agent system (see section 2 above).
Loaded at startup, parsed into ReactionRegistry, used to build routing maps.

### 8. Tests

- Unit: SpecDrivenActor with a simple spec + routing, verify transitions + tells
- Unit: reaction registry parsing from TOML
- Integration (PG): full chain Agent → ContextManager → ToolRouter → Compactor
  with mock integrations, verify state transitions at each step
- Integration (PG): OData endpoint creates actor, dispatches action, reads state

### 9. E2E demo

Update experimental demo:
1. Load IOA specs for all 4 actors
2. Load reaction rules TOML
3. Register SpecDrivenActors with routing maps
4. Register mock integration actors
5. Create session via OData-like call
6. Send StartProcess, watch full chain with state validation

---

## Implementation order

1. Extend temper-spec for typed params
2. Proto codegen from specs (standalone tool)
3. Reaction types in temper-runtime
4. Routing map builder
5. SpecDrivenActor for mailbox model
6. Mock integration actors
7. Reaction rules TOML for agent system
8. OData wiring
9. Tests (unit + integration)
10. E2E demo

---

## Dependencies

- temper-actor-runtime (Phase 1)
- temper-runtime (base crate — reaction types go here)
- temper-spec (IOA parser)
- temper-jit (TransitionTable)
- temper-server (OData layer — adapt, not rewrite)
- prost (proto messages)

---

## Resolved questions

1. **Reaction types** → into temper-runtime
2. **Proto messages** → codegen from specs: extend spec with typed params, generate prost derive structs via standalone tool
3. **Action → message type** → convention: action name = message_type string
4. **Integration actors** → tell() + callback (non-blocking)
5. **Cross-namespace** → deferred, same-namespace only for Phase 2
6. **Actor creation** → via OData POST (existing Temper surface)
7. **Spec purity** → specs don't know routing; reaction rules handle wiring
