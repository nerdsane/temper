# Entity lifecycle (create -> action -> readback)

## Sub-features
Entity creation at the spec's initial state, action dispatch with params, state application, readback. Handlers in `crates/temper-server/src/odata/write.rs`; effect application in the actor runtimes.

## How to get to it (user POV)
An agent creates an entity, moves it through its state machine by dispatching governed actions, and confirms the machine actually moved.

## Driving it
```bash
# create → 201 Created (odata/write.rs StatusCode::CREATED on every success path).
# The body may set id (else the server mints one) AND ordinary initial fields
# (they are retained); only server-derived fields are stripped, and status,
# if given, MUST equal the spec's initial state.
curl -sS -X POST "http://localhost:3600/tdata/<Set>" \
  -H "Authorization: Bearer $KEY" -H "X-Tenant-Id: default" -H "Content-Type: application/json" \
  -d '{"id":"t-1","some_initial_field":"v"}'

# move the state - state variables are set via ACTION PARAMS in the POST body, not create fields
curl -sS -X POST "http://localhost:3600/tdata/<Set>('t-1')/Temper.<Action>" \
  -H "Authorization: Bearer $KEY" -H "X-Tenant-Id: default" -H "Content-Type: application/json" -d '{"amount":5}'

# read back - the transition is only proven here
curl -sS "http://localhost:3600/tdata/<Set>('t-1')" -H "Authorization: Bearer $KEY" -H "X-Tenant-Id: default"
```
Params are delivered as the message payload (`SpecMessage::with_params`) and merged into the entity's fields; the transition table maps `(status, action)` and status moves via the `SetState` effect (or the durably stored `to_status`).

## What proves it
The entity's `status` after the action matches the spec's transition, read back over OData. Create is **201 Created** (`odata/write.rs`). Dispatch codes (`odata/bindings.rs:266-346`): a successful transition is **200**; every `success=false` from `entity_actor/effects.rs` maps to **409 Conflict** `ActionFailed` — not only the from-state miss. `effects.rs` emits three agent-facing strings on that path: known action / wrong state / no guard failure → `Action '<A>' not valid from state '<S>'` (`:366`); known action / guard failed → `render_guard_failure` (`Action '<A>' blocked from state '<S>': guard …`, `:234`); unknown action (including a `Temper.<Action>` that is not on the type) → `Unknown action: …` (`:378`). Drivers that match only the from-state string will mis-classify a 409 `Unknown action: …` as "not an invalid-from-state". Replay: known action from the wrong state → 409 + from-state string; unknown `Temper.<Action>` on a real entity → 409 + `Unknown action: …`. A dispatch on an unregistered type is **404** (`EntityTypeNotGoverned`); a Cedar denial is **403**. Read the entity back regardless - the 200 vs 409 tells you whether the machine moved. For history, `GET /observe/entities/{entity_type}/{entity_id}/history`; to block until a target state, `GET /observe/entities/{entity_type}/{entity_id}/wait`.

## Gotchas
- You cannot create an entity directly into an arbitrary state - create mints it at the declared initial state only.
- Two persistence backends exist: the default event-sourced `EntityActor` (journal + replay, see event-sourcing-readback.md) and, for entity types in `actor_backed_types`, a materialized `temper-actor-runtime` path. They apply the same effects but store differently - name which one you exercised.
- A dispatch on a type with no registered spec returns 404 `EntityTypeNotGoverned` (`DispatchError::Ungoverned`). Entities are created by POSTing to the entity set (`POST /tdata/<Set>`), which mints them at the spec's initial state; the create body's `status`, if present, must equal that initial state (`odata/write.rs`). (Local note: driving create/dispatch end to end needs a spec whose Cedar policy permits your principal - the bootstrapped operator key is denied on the built-in system entities, which return 403 `no matching permit policy`.)
- Cross-entity guards are resolved before dispatch; pass real arrays in action params, not stringified JSON (runtime lore from ARN-92 - verify empirically, it is not a single file:line).
