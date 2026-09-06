# Decisions and tradeoffs

## D18: Reject collection comparisons without a runtime contract

**Decision:** Reject parameter equality and inequality constraints targeting list or set fields when parsing the specification.

**Came up because:** Review reproduced an accepted list constraint whose comparison context never contained the list. Treating lists and sets as interchangeable JSON arrays would also invent equality semantics this contract does not define.

**Options:** Accept the unsatisfied constraint; add collection comparison semantics and another state parameter across every actor path; reject these new constraints before installation.

**Chose parser rejection because:** It makes the supported contract explicit without changing existing list/set actions or guards. The DSF and Effort contracts use scalar comparisons. Collection-valued parameter comparisons remain unavailable rather than appearing supported and refusing every action at runtime.

**Where:** crates/temper-spec/src/automaton/contracts.rs; docs/efforts/ARN-467/spec.md. The four list/set equality/inequality cases were accepted before the fix; all 276 parser tests pass after the rejection was added.

## D16: Compare actual stored values and return an unavailable actor error

**Decision:** Constraints compare persisted pre-action values; fresh contracted actors materialize defaults at creation. Greater-than inputs remain nonnegative integers, and a missing PostgreSQL actor system returns HTTP 503.

**Came up because:** Regressions showed missing recovered fields being replaced by declaration defaults during comparison, negative greater-than inputs passing against signed state, and a generic write panicking when the configured actor backend was absent.

**Options:** Retain validator defaults and assume actor availability; enforce the readable pre-state contract and return the specific unavailable boundary error.

**Chose the explicit boundaries because:** Recovery does not fabricate state, numeric validation matches the declared contract, and unavailable infrastructure does not panic the request handler. Existing unconstrained initialization remains unchanged.

**Where:** crates/temper-jit/src/table/action_contract.rs; crates/temper-server/src/odata/write.rs; crates/temper-server/tests/strict_generic_writes.rs. The new regressions failed before these fixes and pass afterward.

## D13: Reject contracts the validator cannot execute

**Decision:** Reject repeated action names in contracted IOAs, malformed strict integer defaults, and comparison targets without values in the shared validator.

**Came up because:** Review reproduced a later action replacing an earlier action's contract, invalid integer defaults becoming strings, and accepted comparisons against absent runtime metadata.

**Options:** Select contracts by transition rule and pass all runtime metadata into validation. Reject ambiguous names and unsupported references at the parser boundary.

**Chose parser rejection because:** The current source inventory has no repeated action names, and resources use declared fields or identity for comparison. This fixes the defects without another rule-selection or metadata path. Nested constraints, triggers and composite metadata now share one parse pass.

**Where:** crates/temper-spec/src/automaton/contracts.rs; crates/temper-spec/src/automaton/toml_parser/mod.rs; docs/efforts/ARN-467/spec.md. The three new regression cases failed before the fix; all 275 parser tests pass after it.

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


## D8: Authorize PostgreSQL mutations before consulting the input contract

**Decision:** Load the PostgreSQL resource for Cedar authorization, then validate action inputs or refuse unsupported generic writes.

**Came up because:** The second review reproduced a stored-value oracle: a denied caller received 400 for an incorrect compare-and-set guess and 403 for the correct guess. The PostgreSQL generic-write refusal also skipped Cedar.

**Options:** Keep type and constraint checks before authorization; duplicate permission logic; share the PostgreSQL resource authorization boundary before either response.

**Chose shared authorization because:** Unauthorized callers receive the same policy refusal regardless of their guesses, and all mutation attempts reach Cedar. Authorized callers retain the specific contract errors without enqueueing invalid work.

**Where:** crates/temper-server/src/odata/write.rs; crates/temper-server/tests/strict_postgres_actions.rs.

## D9: Remove direct PostgreSQL field updates

**Decision:** Initialize Process fields during idempotent creation and remove the otherwise unused direct field-update API.

**Came up because:** The second review found that update_actor_fields bypasses the specification. Its only caller was Process collection creation, after spawning the registered actors.

**Options:** Add another contract check to an unrestricted mutation API; preserve the API for one creation caller; create Process with its fields before spawning its peers and delete the update API.

**Chose creation followed by deletion because:** Fresh actors receive their fields and declared defaults without granting a way to rewrite existing actors. Repeated creation preserves existing state through the existing insert-on-conflict behavior.

**Where:** crates/temper-actor-runtime/src/system.rs; crates/temper-server/src/odata/write.rs; crates/temper-server/tests/strict_postgres_actions.rs.


## D10: Separate routed source fields from ordinary action inputs

**Decision:** Deliver PostgreSQL emit and trigger reactions in an internal envelope, then project source fields onto the strict target action's declared parameters before checking its constraints.

**Came up because:** The real PostgreSQL cascade test showed that sending every source field as ordinary input makes strict targets reject valid reactions for unrelated source metadata.

**Options:** Exempt every actor-origin message from validation; add the target registry to each sender; identify reaction deliveries explicitly and construct the declared input at the receiver.

**Chose explicit reaction deliveries because:** The target owns its input contract, while ordinary caller and actor messages retain the exact allowlist. Routed requests still satisfy constraints against unmodified target state. An external envelope without an actor sender is rejected, and unknown target actions fail before mutation.

**Where:** crates/temper-actor-runtime/src/spec_actor.rs; crates/temper-actor-runtime/src/pg_strict_tests.rs.

## D11: Prepare generated callbacks without relaxing caller inputs

**Decision:** Remove the unused timer marker and project generated callback payloads onto the target action's declared inputs before the existing strict actor validation.

**Came up because:** Runtime-generated duration, tracing, and failure metadata caused valid strict callbacks to be rejected, while the unused scheduled marker prevented parameterless timers from firing.

**Options:** Sanitize every caller request before validation; allow reserved metadata in every strict action; prepare only generated callbacks at their internal dispatch boundaries.

**Chose internal preparation because:** Public inputs keep their exact allowlist and callback constraints still compare with unmodified target state. A refused callback is surfaced without turning it into an unrelated failure transition on newer state.

**Where:** crates/temper-server/src/state/dispatch/effects.rs; adapter.rs; compensation.rs; wasm/invocation_artifacts.rs; generated_callbacks.rs and native regression tests.

## D12: Preflight strict composite writes in their execution order

**Decision:** Use shared typed initialization and a per-target virtual state to validate every composite sub-write before durable or external effects.

**Came up because:** Composite creation lost declared defaults, normalization inserted an undeclared Id parameter, and independent preflight snapshots could not validate a later write that depends on an earlier write to the same target.

**Options:** Validate only during staging; validate every write against the original snapshot; simulate the same ordered state updates before applying the batch.

**Chose ordered preflight because:** Invalid later inputs leave all target journals and overflow storage untouched, while valid dependent writes retain atomic behavior. Strict sub-write parameters remain explicit inputs; generated identity stays in the resource address and initial state. Data-only creation reuses the same initializer.

**Where:** crates/temper-server/src/state/dispatch/composite.rs and composite/helpers.rs; entity_actor/actor.rs; state/entity_ops.rs and native regression tests.


## D14: Materialize contract defaults and identity at creation

**Decision:** Initialize declared values for fresh strict or constrained actors, and persist standalone PostgreSQL actor identity when the actor is spawned.

**Came up because:** Constraint fallback compared missing stored values with declarations while guards and effects used absent state, and generic PostgreSQL spawn did not persist the Id that constraints accepted as a target.

**Options:** Infer values during validation, synthesize identity on first message, or initialize the actual state at creation and compare only stored values afterward.

**Chose creation-time initialization because:** Constraints, guards, and effects read the same persisted state. Recovered missing values fail rather than silently receiving new defaults. Standalone actor Id is its complete namespace. HTTP creation supplies its canonical Id explicitly and overrides that default. Existing actors retain their state when spawn is repeated, and unconstrained legacy initialization stays unchanged.

**Where:** crates/temper-jit/src/table/action_contract.rs (coordinated root change), crates/temper-actor-runtime/src/actor.rs, system.rs, spec_actor.rs, and native actor initialization.

## D15: Configure Process scratch resets in its component

**Decision:** Configure fields cleared on accepted Process inputs at the existing agent component registration, using a generic reset-fields setting on the spec actor.

**Came up because:** The generic PostgreSQL handler hardcoded the Process entity name, two action names, and application scratch keys. A different Process spec therefore lost data it never declared as transient.

**Options:** Keep the hardcoded behavior, add field deletion to the specification language, wrap the actor and duplicate its validation, or supply an explicit per-action reset configuration.

**Chose explicit configuration because:** The generic handler clears configured fields only after validation succeeds and before it merges parameters and emits integration context. Refused inputs retain state and emit nothing. The existing Process component supplies its own names and keys. Its pre-existing placement in temper-agents remains unchanged and is flagged for repository ownership review.

**Where:** crates/temper-actor-runtime/src/spec_actor.rs, crates/temper-agents/src/lib.rs, and adapter regression tests.


## D17: Resolve comparison values without changing stored fields

**Decision:** Read compared overflow values through the existing bounded, length- and hash-verified blob reader into separate comparison state, and refuse writes that would truncate a declared comparison target.

**Came up because:** An actual native actor writing a 512 KiB value stored a blob reference or a truncation placeholder. Later equality compared that representation with the original value and refused a valid request. A missing blob could also make an inequality appear true without establishing its value.

**Options:** Compare caller-provided descriptors or content hashes, retain all large values inline, hydrate the actor state in place, or resolve only the compared fields before the pure interpreter runs.

**Chose separate verified comparison values because:** Equality and inequality use the same logical stored value without rewriting the actor fields, fetching unrelated blobs, or increasing persistent state limits. Missing, corrupt, or over-budget blobs refuse both comparisons. Native execution, concurrency retries, and composite preflight and staging share this boundary. InlineTruncate refuses an oversized write before effects instead of accepting irreversible data loss on a field needed by a constraint. Previously truncated historic values cannot be recovered from their placeholder.

**Where:** crates/temper-server/src/entity_actor/action_input.rs, effects.rs, actor.rs, blobs/hydration.rs, and state/dispatch/composite.rs. Real local Turso and filesystem-object tests cover equality, inequality, actor restart, missing bytes and forged bytes; the inline-store test proves refusal leaves the full state unchanged.

The same persisted-prestate rule rejects empty stored bytes for strict or constrained PostgreSQL actors. Supported creation writes serialized initial state before accepting messages; an empty recovered byte vector is not that creation event. A focused regression distinguishes empty bytes from valid serialized initial state and preserves the unconstrained legacy behavior. A strict child without a declared initializer also refuses before creation, with an observable parent refusal, so generated parent links are not silently discarded.

Sequential composite comparison also reads bytes generated by earlier sub-writes before falling back to stored objects. These bytes already belong to the pending batch and pass the same length and hash verification. Preflight retains them in its existing ordered batch, and neither preflight nor validation writes an object. The new IOA-backed regression first failed on a matching pending value, then proves both successful ordered execution and zero journal/object writes after a stale comparison.


## D19: Initialize absent PostgreSQL actors and preserve recovered bytes

**Decision:** The PostgreSQL activator initializes only absent rows through the actor's handle-aware initializer and passes existing state bytes unchanged to the handler.

**Came up because:** The fourth review found that activation replaced empty recovered bytes with declaration defaults before the strict handler could refuse them. It also created absent actors without the identity supplied by ordinary spawn.

**Options:** Keep the activator's fallback, add another strict-spec check in the generic activator, or let the actor interpret existing state while sharing its creation initializer.

**Chose the shared actor boundary because:** Recovery cannot fabricate comparison values, absent actors persist identity consistently, and unconstrained actors retain their own handling of empty state. A deterministic refusal still consumes its queue message without changing stored bytes; the queue cursor and row version advance.

**Where:** crates/temper-actor-runtime/src/pg.rs and pg_strict_tests.rs. The real PostgreSQL activator regression failed before the correction and distinguishes empty recovered state from an absent actor addressed by a queued message.


Automatic standalone identity is limited to strict or constrained actors. Explicit HTTP identity remains unchanged. Unconstrained legacy actions still accept and forward their existing input fields on a successful transition. Denied and unknown transitions intentionally stop mutating state for all actors; restoring parameter writes or Process scratch clearing on refusal would restore the defect. A regression checks unchanged legacy initialization, accepted extra fields in emitted context, and byte-for-byte state preservation with no messages after denied or unknown actions.

Context spawning uses the same registered handle-aware initializer before inserting a child row. The full integration hook exposed that `ActorContext::spawn` still inserted empty bytes, while activation correctly treated those bytes as recovery. Passing the existing shared handler registry into the activator and context avoids a second registry or recovery fallback. The regression checks a strict child's identity and declared counter before its first message, then checks that repeated spawn preserves its changed state. An unregistered context spawn returns `NotFound` before inserting a row; lookup still reports only persisted siblings.

## D20: Persist declaration defaults at creation

**Decision:** Record the typed initial values in the kernel bootstrap event, and recover only committed initial values or historical action parameters.

**Came up because:** A new constrained default was invented during journal recovery but stayed absent when recovering the same old entity from a snapshot. The atomic File path also omitted current defaults when creating a fresh File.

**Options:** Reapply the current specification on every recovery; require snapshots; capture the creation values once in the existing bootstrap event.

**Chose the bootstrap event because:** Full replay and snapshots retain the same historical values without making snapshots mandatory or guessing a migration. Old journals retain absent values until a declared action writes them. The atomic File path uses the ordinary initial-state constructor and the same bootstrap serializer.

**Where:** crates/temper-server/src/entity_actor/bootstrap.rs; actor.rs; authoritative_replay_test.rs; crates/temper-server/src/state/file_initial_writes.rs.


The same recovery boundary refuses journal read errors for strict or constrained tables even when snapshot recovery was requested. A failed read cannot establish that a stream is empty. Unconstrained legacy lenient recovery retains its existing behavior; the injected read-error regression covers all three modes.

Composite creation uses the same bootstrap serializer. Its separate envelope builder had omitted the committed defaults, so replay produced revision 2 after two increments instead of 5 from the declared initial revision 3. The composite regression now checks the stored typed values and both recovery paths after declaration defaults change; the sequential and seeded composite tests retain their original expected values.

## D21 — Resolve simulated and legacy blob comparisons at the shared read boundary

**Decision:** Retain generated overflow bytes in blob-enabled simulations and allow default-tenant actors to use the existing bounded legacy database read capability.

**Came up because:** The review found that simulation discarded overflow bytes and native action comparison could not read an object that remained only in the legacy database. Regressions reproduced both failures with a 512 KiB stored value. The public PostgreSQL creation helper also bypassed the existing strict creation validator.

**Options:** Compare serialized descriptors, bypass overflow in simulation, retain every generated blob forever, or use the production comparison preparation with an in-memory source and the existing bounded database fallback. For creation, rely only on HTTP validation or validate the public creation helper itself.

**Chose the shared preparation over descriptor comparison because:** Equality and inequality must use verified logical bytes. Simulation keeps only bytes referenced by current fields, retains its existing inline default, and explicitly selects blob mode when testing that production storage shape. The database capability is passed only for the default tenant; other tenants cannot read global legacy objects. PostgreSQL actors expose a small creation validation hook, permissive for unrelated actor implementations and delegated to the existing strict table validator by spec-driven actors.

**Where:** `crates/temper-server/src/entity_actor/sim_handler.rs`, `crates/temper-server/src/blobs/read_source.rs`, `crates/temper-server/src/state/entity_ops.rs`, and `crates/temper-actor-runtime/src/system.rs`; PR #456.


## D22 — Discover addressed actors through their pending mailbox messages

**Decision:** Let the PostgreSQL scheduler discover registered actors with pending messages before their instance row exists, and remove the Process-name exception from generic HTTP creation.

**Came up because:** A strict entity named Process created every registered actor in its namespace. Removing that exception alone stranded the standard Process chain in PreparingContext: the activator could initialize an absent actor, but scheduler discovery never selected it.

**Options:** Add an application-specific creation registry; preserve the Process string exception; or discover pending mailboxes and use the existing registered handler and activator.

**Chose mailbox discovery because:** The existing activator already owns initial state and atomic cursor processing. Discovery now includes absent instances with cursor zero, still groups by the same namespace/type identity, filters registered types, and keeps the configured batch bound. The existing mailbox index and instance/type primary keys serve the joins; discovery may examine more historical message rows, so this avoids adding a new registry at the cost of that query work. Concurrent polling and restart tests assert one completed Process turn and one context preparation. Empty-object actor lookups also retain their cached fast path, while any nonempty creation input is validated before a cached result is returned.

**Where:** `crates/temper-actor-runtime/src/schema.rs`, `crates/temper-server/src/odata/write.rs`, `crates/temper-agents/tests/agent_chain.rs`, and `crates/temper-server/tests/strict_postgres_actions.rs`; PR #456.
