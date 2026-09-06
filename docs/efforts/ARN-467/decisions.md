# Decisions and tradeoffs

## D1: Enforce contracts in the kernel

**Decision:** Add opt-in strict action parameters and pre-state constraints to IOA and the production actor boundary.

**Came up because:** Actual simulator and HTTP tests showed that an observation could overwrite an undeclared desired field and generic writes could bypass named actions.

**Options:** Depend on Cedar action names alone. add DSF-specific HTTP checks. enforce the declared contract in shared kernel execution.

**Chose shared kernel execution because:** It protects both HTTP and internal dispatch and lets the real simulator prove the behavior. The opt-in flag preserves existing applications until their specifications adopt the contract.

**Where:** crates/temper-jit/src/table/action_contract.rs. crates/temper-server/tests/strict_action_contract.rs. crates/temper-server/tests/strict_generic_writes.rs.

## D2: Reject numeric strings

**Decision:** Numeric constraints accept JSON integers only.

**Came up because:** A factory regression showed a numeric string could pass the new timestamp comparison while the existing numeric effect skipped it.

**Options:** Coerce action payloads globally. accept strings only in comparisons. use the same integer representation as effects.

**Chose integer representation because:** It removes the guard/effect disagreement without silently changing payload values or unrelated applications.

**Where:** crates/temper-jit/src/table/action_contract.rs.


## D3: Initialize strict entities from their declarations

**Decision:** Strict entities initialize counters, booleans and fields from the IOA declarations, and constraints use those same values.

**Came up because:** Independent review and four reproduced actor regressions found that empty maps made the first observation fail and nonzero defaults disagree with guards.

**Options:** Insert zero-value effects in every app. assume zero only in comparisons. initialize strict actors once from the declared values.

**Chose declared initialization because:** Native execution, PostgreSQL execution and simulation can use the same defaults without application-specific effects. Existing applications retain their current initialization until opting into strict mode.

**Where:** crates/temper-jit/src/table/action_contract.rs; crates/temper-server/src/entity_actor/actor.rs; crates/temper-server/tests/strict_action_contract.rs.


## D4: Initialize spawned strict children through declared parameters

**Decision:** A spawn effect creates a strict child with identity only and projects the generated initializer payload onto that child action's declared parameters.

**Came up because:** Review found that the spawn adapter unconditionally supplied undeclared parent metadata to generic child creation, which strict specifications reject.

**Options:** Exempt internal writes from the contract. reject all strict child spawns. construct the declared initializer input from the spawn data.

**Chose the declared initializer because:** It preserves child creation and explicit parent links while enforcing the same input contract on the child. A strict child declares the parent fields it needs on its initialization action.

**Where:** crates/temper-server/src/state/dispatch/cross_entity.rs; crates/temper-server/tests/strict_generic_writes.rs.


## D5: Consume deterministic PostgreSQL refusals

**Decision:** PostgreSQL input refusals consume their queue message while preserving actor bytes and discarding buffered messages; retryable handler failures still roll back.

**Came up because:** Returning HandlerFailed for strict validation left the rejected message at the front of the FIFO queue, preventing later valid work.

**Options:** Retry every refusal. silently report success. distinguish a rejected input from a retryable execution failure.

**Chose typed ActorError::Rejected because:** It preserves refusal and queue continuity without storing a result per request. Strict PostgreSQL HTTP requests validate before enqueue and return 202 with the message ID. Actual execution runs through the scheduler and must be read back; activating an arbitrary queued message cannot prove the newly submitted request completed.

**Where:** crates/temper-actor-runtime/src/actor.rs; crates/temper-actor-runtime/src/pg.rs; crates/temper-actor-runtime/src/spec_actor.rs; crates/temper-server/src/odata/write.rs.

## D6: Refuse unsupported PostgreSQL generic writes before native lookup

**Decision:** Return the strict action contract refusal for PostgreSQL-backed PATCH, PUT, and DELETE before consulting the native actor index.

**Came up because:** The real PostgreSQL HTTP proof created an Order successfully, then received 404 for generic edits because the native-only existence lookup ran before the strict type check.

**Options:** Report the existing PostgreSQL entity as missing; add generic PostgreSQL CRUD; reject unsupported strict-type verbs before that lookup.

**Chose the early type check because:** Strict types accept changes through their declared actions, so generic CRUD adds no required capability. The PostgreSQL-only check gives callers the correct 405 response while leaving native and legacy authorization behavior unchanged.

**Where:** crates/temper-server/src/odata/write.rs and tests/strict_postgres_actions.rs. The test starts a real loopback HTTP server with PostgreSQL storage and also proves invalid inputs do not enqueue, accepted actions return 202 with a message ID, and state changes only after activation.

## D7: Preserve declared fields during PostgreSQL creation

**Decision:** Merge creation fields into the actor's initialized fields instead of replacing them.

**Came up because:** The actual PostgreSQL HTTP test showed that collection creation always adds identity and status fields, and spawn_with_fields replaced the declared string defaults with that identity object.

**Options:** Reconstruct defaults in every HTTP caller; discard identity fields; honor spawn_with_fields' existing merge contract at the shared creation boundary.

**Chose the shared merge because:** The actor retains its declared defaults while creation still supplies identity and any permitted initial values. The real HTTP test now reads the default before enqueueing or executing an action.

**Where:** crates/temper-actor-runtime/src/system.rs and crates/temper-server/tests/strict_postgres_actions.rs.
