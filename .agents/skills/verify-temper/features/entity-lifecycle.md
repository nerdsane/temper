# Entity lifecycle (create -> action -> readback)

## Sub-features
Entity creation at the spec's initial state, action dispatch with params, state application, readback. Handlers in `crates/temper-server/src/odata/write.rs`; effect application in the actor runtimes.

## How to get to it (user POV)
An agent creates an entity, moves it through its state machine by dispatching governed actions, and confirms the machine actually moved.

## Driving it
```bash
# create - the body may set id (else the server mints one); status, if given, MUST equal
# the spec's initial state. Server-derived fields are stripped.
curl -sS -X POST "http://localhost:3600/tdata/<Set>" \
  -H "Authorization: Bearer $KEY" -H "X-Tenant-Id: default" -d '{"id":"t-1"}'

# move the state - state variables are set via ACTION PARAMS in the POST body, not create fields
curl -sS -X POST "http://localhost:3600/tdata/<Set>('t-1')/Temper.<Action>" \
  -H "Authorization: Bearer $KEY" -H "X-Tenant-Id: default" -d '{"amount":5}'

# read back - the transition is only proven here
curl -sS "http://localhost:3600/tdata/<Set>('t-1')" -H "Authorization: Bearer $KEY" -H "X-Tenant-Id: default"
```
Params are delivered as the message payload (`SpecMessage::with_params`) and merged into the entity's fields; the transition table maps `(status, action)` and status moves via the `SetState` effect (or the durably stored `to_status`).

## What proves it
The entity's `status` after the action matches the spec's transition, read back over OData. A 200 on dispatch is NOT proof: an action that is not valid from the current state still returns 200 while logging "action not valid from current state" and leaving status unchanged. For history, `GET /observe/entities/{entity_type}/{entity_id}/history`; to block until a target state, `GET /observe/entities/{entity_type}/{entity_id}/wait`.

## Gotchas
- You cannot create an entity directly into an arbitrary state - create mints it at the declared initial state only.
- Two persistence backends exist: the default event-sourced `EntityActor` (journal + replay, see event-sourcing-readback.md) and, for entity types in `actor_backed_types`, a materialized `temper-actor-runtime` path. They apply the same effects but store differently - name which one you exercised.
- A dispatch on a type with no registered spec is default-deny (`DispatchError::Ungoverned`). Bare POSTs create entities lazily.
- Cross-entity guards are resolved before dispatch; pass real arrays in action params, not stringified JSON (runtime lore from ARN-92 - verify empirically, it is not a single file:line).
