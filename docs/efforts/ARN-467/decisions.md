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

The full workspace run also reached data-only creation, whose separate event serializer still omitted committed defaults. It now uses the existing bootstrap serializer; its HTTP regression checks the stored field, counter, boolean and list values before hydration. The lenient replay metric fixture now journals its initial Customer value before the malformed update and supplies a different constructor value. Its unchanged assertion therefore proves that replay preserves committed history, without treating an uncommitted constructor value as history.

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

## D23 — Keep authorization independent of request stack depth

**Decision:** Evaluate Cedar on a fixed eight-megabyte stack and deny with an engine error if Cedar still reports a recursion limit.

**Came up because:** The same installed policy and authenticated identity allowed a collection read through the direct authorization endpoint but denied it through OData. A synthetic one-megabyte-thread regression reproduced the missing permit and also showed a matching long forbid being skipped while a short permit allowed the request.

**Options:** Add overlapping application permissions, increase every server thread's stack, or isolate the existing Cedar evaluation call with the library's documented stack-growth mechanism.

**Chose the evaluation boundary because:** It keeps the policy's meaning independent of HTTP call depth without adding authority or increasing every thread's stack allocation. Evaluation uses up to an eight-megabyte stack and a remaining recursion-limit diagnostic refuses the request, even if another policy permitted it. Other Cedar evaluation-error behavior is unchanged.

**Where:** crates/temper-authz/src/engine/mod.rs; engine/stack_tests.rs; crates/temper-authz/Cargo.toml.

## D24 — Authorize collection creation before strict-field validation

**Decision:** Apply Cedar to the proposed creation before returning strict-contract field errors.

**Came up because:** The fresh panel found that an unauthorized caller could distinguish an identity-only body from a forbidden extra field by comparing 403 and 400 responses.

**Options:** Keep schema validation first, obscure validation responses, or authorize the prepared resource before checking its strict field contract.

**Chose authorization first because:** It applies the same ordering as declared actions while retaining the prospective fields that attribute-based creation policies need. Verification and initial-status errors use the same ordering because they also disclose the specification. Authorized invalid requests still fail before actor creation or journal changes.

**Where:** crates/temper-server/src/odata/write.rs; tests/strict_creation_boundaries.rs.

## D25 — Reject constraints whose declared values have no supported runtime meaning

**Decision:** Validate numeric defaults for every contracted actor and restrict comparison field types to strings, booleans and integers.

**Came up because:** A constrained non-strict actor accepted a negative counter default and materialized zero. The parser also accepted float and number comparisons although initialization retained those defaults as strings and numeric comparison required JSON integers.

**Options:** Expand the runtime to support more numeric and structured types, accept impossible contracts, or reject unsupported declarations before installation.

**Chose parser rejection because:** The existing contract and DSF callers use strings, booleans and integers. Installation must agree with initialization and comparison instead of silently substituting values or adding another numeric representation. Unconstrained non-strict specifications keep their existing default parsing.

**Where:** crates/temper-spec/src/automaton/contracts.rs; docs/efforts/ARN-467/spec.md.

## D26 — Distinguish skipped history from a new actor

**Decision:** Require an empty durable sequence before writing a new bootstrap, and apply the committed-default position check only to events that contain committed defaults.

**Came up because:** Two regressions reproduced the review's restart failure. Lenient replay refused a legacy Created event at sequence 2 after skipping an incompatible event. Startup also mistook a journal containing only skipped events for a new actor and appended committed defaults after that history.

**Options:** Reject all legacy layouts, permit committed defaults anywhere in a journal, or distinguish an empty history from an empty count of successfully decoded events.

**Chose the durable sequence because:** It prevents startup from inventing a new creation fact after existing history. Legacy lenient replay retains its previous behavior, while authoritative replay and the position requirement for committed defaults remain strict.

**Where:** crates/temper-server/src/entity_actor/actor.rs; bootstrap_recovery_test.rs.

## D27: Keep one owner of the libSQL connection close

**Decision:** Vendor the published libSQL 0.9.29 crate with its redundant outer connection destructor removed, and run the concurrent lifetime regression in temper-store-turso.

**Came up because:** The required kernel push checks aborted in native libSQL. A regression against the exact published crate reproduced the abort; the six-line deletion passed 160,000 connection lifecycles and the previously failing 978-test server suite.

**Options:** Change the storage contract, depend on a private fork that public builds cannot fetch, create another publicly hosted dependency, or retain a reproducible source copy with the minimal patch.

**Chose the source copy because:** The inner Connection already owns closure. Removing the outer close preserves the storage API and makes the correction available to every build without extra credentials or another repository. The storage crate uses a direct path dependency because a root-only Cargo patch would not propagate to downstream git consumers. The first-party placeholder hook excludes exactly this upstream directory; its regression proves adjacent vendor paths and first-party source remain checked. The cost is maintaining the vendored source and its provenance.

**Where:** vendor/libsql/src/local/impls.rs; crates/temper-store-turso/tests/connection_lifetime.rs; docs/adrs/0175-libsql-connection-lifetime.md.

## D28: Run entity reactions after inline WASM callbacks

**Decision:** Dispatch inline WASM callbacks through complete typed dispatch and await both nested integrations and entity reactions.

**Came up because:** A real provider collection committed its callback, but the callback's entity trigger never created the immutable observation. Inline callbacks called core dispatch directly; background callbacks used complete dispatch.

**Options:** Change callers to background mode, duplicate reaction logic in WASM callback handling, or use the existing complete dispatch through its boxed recursion boundary.

**Chose complete dispatch because:** Both modes retain the same post-commit behavior. Inline requests wait for their dependent reactions, and rejected callbacks still produce no reaction. The real HTTP/WASM regression reproduces the missing target transition before the fix.

**Where:** crates/temper-server/src/state/dispatch/wasm/{boxed.rs,invocation_artifacts.rs}; tests/strict_native_callbacks.rs.

## D29: Refuse unavailable PostgreSQL writes and authorize before verification details

**Decision:** Keep PostgreSQL-backed POSTs on their configured runtime, and authorize bound actions and generic entity writes before returning verification status.

**Came up because:** Review identified two PostgreSQL POST paths that could fall through to native actors when the runtime was absent, plus verification responses returned before Cedar authorization. Actual HTTP regressions reproduced native creation and an unauthorized 423 response.

**Options:** Keep conditional runtime dispatch and the early verification check, or refuse the absent runtime and move verification checks after each existing authorization boundary.

**Chose explicit refusal and authorization ordering because:** Missing runtime configuration cannot create entities in another store. Authorized callers still receive verification refusals, while denied callers receive Cedar denials.

**Where:** crates/temper-server/src/odata/{write.rs,bindings.rs}; tests/strict_generic_writes.rs.

## D30: Authorize absent entities without creating them

**Decision:** Build an absent entity's Cedar attributes from its declared initial state without spawning an actor; create it only after authorization and verification pass.

**Came up because:** Review of D29 showed that reading the authorization snapshot created an absent actor and wrote its bootstrap before refusing an unverified action. The regression observed one actor where zero were required.

**Options:** Restore the early verification response, predict the future bootstrap sequence, or authorize a non-creating initial snapshot and compare its attributes after materialization.

**Chose the non-creating snapshot because:** Denied and verification-blocked requests leave the index and journal unchanged. Successful requests retain the real sequence-based concurrency check; a competing creation with different authorization attributes returns a conflict. Storage read errors propagate instead of treating an unavailable store as an absent entity.

**Where:** crates/temper-server/src/state/entity_ops.rs; crates/temper-server/src/odata/{bindings.rs,stream_put.rs}; tests/strict_generic_writes.rs.

D30 caller review: Generic stream uploads also need to materialize their authorized preview before deriving the actual sequence precondition. Both HTTP callers now share this step. Stream callbacks that return success=false produce 409 rather than an apparent successful upload. Read-only publication checks and existing-entity mutation callers retain non-creating snapshot reads.

## D31: Authorize native-runtime delivery with the existing DST limitation recorded

**Decision:** Proceed with ARN-467 native-actor delivery under Rita's explicit exception to the DST marker rule, while retaining the DST-INCOMPLETE assessment and tracking interpreter consolidation in ARN-179.

**Came up because:** The required reviewer found that PostgreSQL SpecDrivenActor has a separate effect interpreter. This predates D28-D30 and prevents a passing marker under .agents/agents/dst-reviewer.md, although the current callback, authorization and stream regressions pass.

**Options:** Consolidate the PostgreSQL interpreter before this delivery, or accept the documented existing limitation for native-actor delivery and keep consolidation open.

**Chose the scoped exception because:** Rita answered “Authorize” to the explicit exception request on 2026-09-08. The delivered runtime uses native actors; this authorization neither declares PostgreSQL effect parity nor changes the DST-INCOMPLETE verdict. Required correctness tests, normal hooks and release reviews still run. ARN-179 remains open with the duplicated interpreter evidence.

**Where:** .agents/agents/dst-reviewer.md; crates/temper-actor-runtime/src/spec_actor.rs; crates/temper-server/src/entity_actor/effects.rs; Linear ARN-179.


## D32: Complete callbacks and validate before creating entities

**Decision:** Apply the same full callback dispatch to native adapters and WASM, validate Commons and write guards before materializing an absent target, authorize composite creation with authoritative identity aliases, and return queue acknowledgements for constrained PostgreSQL actions.

**Came up because:** The final review reproduced a missing adapter reaction, a rejected Commons action leaving an entity behind, missing Id/Status attributes in strict composite authorization, and a constrained non-strict PostgreSQL action reporting synchronous success without a committed result.

**Options:** Add caller exceptions, retain the incomplete paths, or apply the established callback and authorization contracts to every affected caller.

**Chose the shared contracts because:** Callback types now finish their declared reactions consistently; refusals leave no target entity; identity cannot be supplied by untrusted composite parameters; and an accepted queue entry is not presented as a successful transition. Shared PostgreSQL test setup also serializes schema creation, while test-only source naming makes the existing CI scanner classify fixtures correctly.

**Where:** crates/temper-server/src/odata/{bindings.rs,write.rs}; crates/temper-server/src/state/dispatch/{adapter.rs,composite/helpers.rs}; tests/strict_postgres_actions.rs; tests/strict_generic_writes/authorization.rs.


## D33: Preserve concurrent creation and authorize stream refusals

**Decision:** Reload the PostgreSQL row after an idempotent insert, and authorize both new File and generic stream uploads before reporting verification status.

**Came up because:** A deterministic interleaving test showed activation overwriting fields committed by concurrent creation; the stream HTTP regression returned verification status to a denied caller.

**Options:** Serialize all creators behind another lock, preserve the stale synthesized state, or read the row that actually won insertion; retain the early stream gate or move it behind authorization.

**Chose committed-row reads and authorization first because:** PostgreSQL already arbitrates conflicting inserts, so its durable row supplies the correct state without adding another locking protocol. Stream storage failures now return a read error instead of a misleading missing-resource response. The hook exclusion regression now runs in CI, and creation tests prove an authorized caller cannot choose a non-initial status.

**Where:** crates/temper-actor-runtime/src/pg.rs; crates/temper-actor-runtime/tests/integration/creation_race.rs; crates/temper-server/src/odata/stream_put.rs; .github/workflows/ci.yml.


## D34: Validate first actions before creation and preserve authorization through delivery

**Decision:** Check absent targets' input contracts before materialization, carry the PostgreSQL row version used for Cedar on queued external actions, retain bootstrap events for contracted composite targets, and normalize creation identity aliases.

**Came up because:** The 820c89a3 review produced regressions for rejected bound actions and child initializers leaving actors behind, a queued action executing after ownership changed, a parent-gated Ref losing its counter default on recovery, and public PostgreSQL creation storing different Id and id values.

**Options:** Accept these as legacy behavior, remove the affected persistence paths from the release, or enforce the existing creation, authorization and recovery contracts at their boundaries.

**Chose boundary enforcement because:** Rita already answered the scope assessment by retaining the complete persistence contract. Preflight prevents invalid creation while execution-time checks remain authoritative. A version read in the same query as Cedar's state is compared under the activation lock before any handler runs; stale delivery consumes only its queue entry. Contracted composite creation persists its defaults. A supplied identity alias sets both spellings, while conflicting aliases are refused. The PostgreSQL version is carried in the existing protobuf envelope, requiring no mailbox schema migration.

**Where:** crates/temper-server/src/odata/{bindings.rs,write.rs}; crates/temper-server/src/state/dispatch/{cross_entity.rs,composite.rs}; crates/temper-actor-runtime/src/{system.rs,spec_actor.rs,pg.rs}; simulator-backed creation and replay tests and the PostgreSQL FIFO authorization regression.


## D35: Bound integration callback chains with the existing reaction depth budget

**Decision:** Carry callback depth through native dispatch context and refuse nested WASM or adapter callbacks when the existing eight-level reaction budget is exhausted.

**Came up because:** A local self-callback WASM regression overflowed its process stack before reaching a seventy-transition spec guard. Integration callbacks started full dispatch without retaining a depth budget.

**Options:** Restore incomplete callback dispatch, allow unbounded integration chains, or preserve full dispatch while carrying the existing runtime depth bound across callbacks and service-context inheritance.

**Chose a shared bound because:** Both inline and background integrations retain the budget without a global counter or storage change. Exhaustion records the existing callback-refusal event and does not invoke another transition or compensation. Request headers cannot set the runtime depth. The ordinary reaction traversal keeps its existing independent bound. The depth-limited test still overflowed its small test-thread stack, so callback construction and polling also use the existing stacker dependency at their recursive boundary: an 8 MiB segment is allocated only below a 2 MiB reserve. This preserves single-threaded polling and awaited callback ordering; it costs bounded additional stack memory rather than introducing child tasks.

**Where:** crates/temper-server/src/request_context.rs; crates/temper-server/src/state/dispatch/{adapter.rs,wasm/invocation_artifacts.rs}; tests/strict_native_callbacks/budget.rs.


## D36: Close review findings without reopening settled persistence contracts

**Decision:** Enforce the reviewed libSQL source manifest in CI, refuse storage-crate registry publication that would lose the patch, remove the unused actor-name list, and retain the existing documented recovery and performance choices.

**Came up because:** The final panel identified an unenforced vendor patch boundary and a Cargo publication path that substitutes unpatched registry code. It also repeated concerns already covered by D17/D19, D22, D23 and the answered D31 exception.

**Options:** Add more recovery fallbacks and cache layers, split persistence out of the accepted factory scope, or fix demonstrated defects and record the remaining tradeoffs explicitly.

**Chose explicit boundaries because:** The six correctness findings are covered by D34/D35 regressions. Shared table predicates and PostgreSQL fixtures replace duplicated checks; behavior-based test names replace review-round names. The manifest makes vendor drift a CI failure, while publication metadata prevents a release without the fixed dependency. Empty persisted contracted state remains a refusal, not implicit initialization. Pending-message discovery, cold existence reads and composite preflight retain their measured or documented costs; this review did not demonstrate a correctness failure in those paths. Future bootstrap-schema changes require an explicit compatibility decision. Routed in-process messages are trusted IPC, not authenticated capabilities. Timing and stack-threshold tests retain their stated build-dependent limits; the native-runtime DST exception remains unchanged. The existing temper-agents crate still mixes app support into the kernel repository; this effort removes the unused list without relocating that legacy crate.

**Where:** vendor/libsql/TEMPER-PATCH.md; vendor/libsql/TEMPER-SHA256SUMS; scripts/check-vendored-libsql.py; .github/workflows/ci.yml; crates/temper-store-turso/Cargo.toml; crates/temper-agents/src/lib.rs; D17, D19, D22, D23, D31, D34 and D35 above.

## D37: Keep authorization versions consistent across every state writer

**Decision:** Increment the persisted version on auxiliary state updates, restrict child creation preflight to absent children, and match composite preflight's bootstrap count to staging.

**Came up because:** Review found that ActorContext's auxiliary upsert could replace state without invalidating queued authorization. The new child preflight also compared existing blob descriptors against logical values before the actor could hydrate them. Both failures were reproduced locally.

**Options:** Add another locking protocol and blob hydration path, or reuse the existing version guard and actor validation boundaries.

**Chose existing boundaries because:** Every existing-row state writer now advances the version, so queued authorization and concurrent activation CAS detect an auxiliary write. Existing child actors retain hydrated execution-time validation; absent children still reject invalid initialization before materialization. Composite event budgets now count the same Created event that staging retains for contracted targets.

**Where:** crates/temper-actor-runtime/src/actor.rs and pg_strict_tests.rs; crates/temper-server/src/state/dispatch/cross_entity.rs and composite.rs; tests/strict_generic_writes/creation.rs.

## D38: Enforce the same parameter and creation contract at every constructor

**Decision:** Reject incompatible inferred parameter types and non-string strict identities, and resolve generic-write contracts through the same registry-first/static-table lookup as actor creation.

**Came up because:** The fresh fa932974 review found that named parameters could require mutually exclusive types, Id/id accepted arbitrary JSON, and with_specs-backed HTTP paths missed the registry-only strict gate.

**Options:** Let every runtime reject impossible inputs later; special-case individual callers; or validate the shared declaration and creation boundaries with the existing table fallback.

**Chose shared boundaries because:** Invalid contracts fail before installation and strict identity values fail before persistence. HTTP constructors retain Cedar authorization before exposing contract failures. The static HTTP fixture was corrected to carry authenticated context before its baseline comparison; its initial 401 was not contract evidence.

**Where:** crates/temper-spec/src/automaton/contracts.rs; crates/temper-jit/src/table/action_contract.rs; crates/temper-server/src/odata/write.rs; tests/strict_generic_writes.rs.

## D39: Preserve queue acknowledgments and execution budgets during recovery

**Decision:** PostgreSQL bound actions acknowledge enqueueing with a message ID; compensation inherits the originating callback budget; a closed cached actor can be replaced under the existing spawn lock.

**Came up because:** Non-strict PostgreSQL actions discarded activation errors and could report completed execution for a stale queued action. Background compensation reset depth to zero. A failed journal activation left a closed actor reference cached while hot retries prevented passivation.

**Options:** Remove version authorization, retry actions under new authority, evict all timed-out actors, or preserve the existing security and concurrency contracts while correcting response and lifecycle boundaries.

**Chose the existing contracts because:** FIFO delivery does not prove the submitted message ran; an enqueue acknowledgment is the accurate result. Compensation remains bounded without changing its service principal. Only closed mailboxes permit replacement, preserving healthy actors on timeout and keeping the durable entity index. D5/D34 deliberately advance cursor and version on deterministic refusal; this is not changed. D23's unconditional Cedar stack isolation remains intentional. The inherited random simulator's empty payload generation is a coverage limitation; current contract proofs use explicit payloads and are not represented as random coverage. D31's native DST exception remains unchanged.

**Where:** crates/temper-server/src/odata/write.rs; state/dispatch/compensation.rs; state/entity_ops.rs; crates/temper-runtime/src/actor/actor_ref.rs; mailbox/mod.rs.

## D40: Keep dispatch unit tests outside the production module

**Decision:** Move the existing dispatch unit tests unchanged to dispatch_test.rs.

**Came up because:** Passing the parent callback context added one line to a 500-line production file and the normal push readability gate refused it. The file included 67 lines of unit tests.

**Options:** Relax the baseline, compress production code, or separate the existing tests using the repository's test-module convention.

**Chose test separation because:** It preserves readable production code and all test behavior without increasing the allowed readability debt.

**Where:** crates/temper-server/src/state/dispatch/mod.rs; crates/temper-server/src/state/dispatch/dispatch_test.rs.


## D41: Validate absent dispatches and static child specifications at the existing boundary

**Decision:** Resolve generated child specifications through the shared registry-first/static-table lookup, and preflight invalid first actions before core dispatch materializes an actor.

**Came up because:** The bfc1 review found that with_specs/with_storage_stack children missed strict handling, and direct public dispatch could create an actor and bootstrap before rejecting its input. Both regressions fail on the previous implementation.

**Options:** Restrict supported constructors, add a new authorization layer to core dispatch, or apply the same input contract before creation using the existing noncreating snapshot.

**Chose existing boundaries because:** Static and registry-backed children now share declared initialization and observable refusal. Core validates only absent targets against declared defaults; existing or durably persisted targets still validate hydrated execution-time values. Core and native EntityActor do not themselves evaluate Cedar: HTTP and reactions authorize before calling core. This change preserves that authority model and its denial precedence, without inconsistently adding authorization only to absent targets.

**Where:** crates/temper-server/src/state/dispatch/actions.rs; cross_entity.rs; tests/strict_generic_writes/creation.rs.


## D42: Retain explicit parameter types through compilation and serialization

**Decision:** Validate present explicitly typed parameters on strict or constrained actions, preserving omission and bare-name constraint semantics.

**Came up because:** The bfc1 review found that compilation retained only parameter names. A serialized table accepted wrong types even when a parameter explicitly declared uint64; the native simulation also accepted a number for a string at seed zero.

**Options:** Infer all bare-name parameters as strings, rely on optional comparison constraints, or preserve the explicit declaration separately from its allowed name.

**Chose explicit declarations because:** Existing bare-name numeric constraints continue to work, while typed values are checked before state changes and effects in native and PostgreSQL execution. The supported scalar vocabulary follows existing declarations and comparison types: string/status, bool, int/integer, counter/uint64. Signed integers must fit i64; natural numbers must fit u64. Unsupported types and duplicate names fail installation for contracted actions. Legacy uncontracted actions retain their semantics. Table serialization carries the explicit type map; missing parameters stay optional unless required by constraints.

**Where:** crates/temper-spec/src/automaton/contracts.rs; crates/temper-jit/src/table/action_contract.rs; table/builder.rs; crates/temper-server/tests/strict_action_contract.rs; crates/temper-actor-runtime/src/tests/spec_actor_strict/typed.rs.


**D41 review follow-up:** The initial early refusal skipped dispatch failure telemetry. Refused first inputs now use the existing failed-response pipeline, which records metrics and a trajectory then returns before entity effects. A regression checks exactly one failed metric and persisted SQLite trajectory while the actor/index and simulation journal/snapshot remain absent. The spec states parameter-contract refusal specifically; this change does not preflight transition guards.

## D43: Separate parameter aliases from state representations and parse once

**Decision:** Reject uint64 state-field comparisons, propagate typed-parameter decode errors, and compile native actor transitions directly from their parsed automaton.

**Came up because:** The next Grok review found D42's uint64 parameter alias also enabled comparisons against string-backed uint64 state fields. Two inherited defects let invalid typed tables fall back to bare parameter names and gave native actor construction two competing specification inputs.

**Options:** Add uint64 state storage throughout the engine, preserve permissive table parsing and duplicate source inputs, or enforce the existing representations at their parser and constructor boundaries.

**Chose the existing representations because:** Typed uint64 parameters retain numeric validation and can compare against actual counters. Unsupported state comparisons fail installation instead of comparing numeric strings. Typed tables must decode correctly. The actor constructor has one specification, so its transitions and initial state cannot diverge through a second parse. The redundant source argument is removed and its caller migrated.

**Where:** crates/temper-spec/src/automaton/contracts.rs; toml_parser/inline.rs and mod.rs; crates/temper-actor-runtime/src/spec_actor.rs and tests/spec_actor_strict/typed.rs.

**Review dispositions:** Grok's single-table and non-string typed-shape examples reproduce the fallback. Its trailing-comma example was already refused by a later strict TOML metadata parse. Ordinary from_ioa already reports the first parse error; the constructor panic requires inconsistent inputs or changed parsing conditions and is not necessarily a process abort. The final Grok output reports divergence and is not a passing review. D31's native-actor DST exception, five findings and ARN-179 remain unchanged.


## D44 — Qualify the official Turso engine for dependency cleanup

**Decision:** Replace the maintained libSQL patch with current official Turso packages, subject to preserving the existing storage contract.

**Came up because:** Rita rejected both vendored libSQL and a separate fork and explicitly selected the newer Turso tooling.

**Options:** Keep the vendor patch; move the patch to a fork; restore the known failing package; qualify current official Turso packages.

**Chose current Turso over a maintained libSQL patch because:** It keeps database implementation maintenance upstream. Qualification must preserve existing data and local/remote behavior; an engine incompatibility will be reported without starting another upstream repair project.

**Where:** ADR-0176; crates/temper-store-turso; codex/arn467-turso-engine.


## D45 — Keep local and remote Turso behind the storage adapter

**Decision:** Use the official embedded engine for local files and the official serverless client for remote URLs, with one private adapter for the SQL operations the store uses.

**Came up because:** The two official packages expose separate Rust connection and row types; replacing only the local package would drop working remote support or preserve libSQL.

**Options:** Keep libSQL for remote connections; duplicate all storage queries; change remote writes into local-first sync; adapt the two official packages at the existing store boundary.

**Chose the private adapter because:** It preserves one set of storage queries and direct remote-write semantics without database implementation code in Temper. The embedded package's default allocator and full-text-search features are disabled because Temper controls its allocator and does not use those features.

**Where:** crates/temper-store-turso/src/driver.rs; ADR-0176; PR457.

## D46 — Start local queries before returning rows

**Decision:** Prime local query results in the private driver adapter and retain the first row until the caller reads it.

**Came up because:** The new engine defers execution until `Rows::next`; the previous driver started the statement inside `query`. The regression test `query_executes_configuration_even_when_rows_are_discarded` failed with user_version 0 instead of 17. Temper discards results from configuration queries.

**Options:** Change individual configuration callers; patch the upstream engine; preserve query execution semantics in the existing private adapter.

**Chose the adapter over caller changes or an engine patch because:** It preserves the same contract for every store query without a fork or scattered exceptions. The adapter buffers one row and marks exhausted results so subsequent reads cannot restart a completed statement.

**Where:** `crates/temper-store-turso/src/driver.rs`; `crates/temper-store-turso/src/driver/tests.rs`; PR https://github.com/nerdsane/temper/pull/457.

## D47 — Close remote streams when their connection leaves scope

**Decision:** The private connection adapter schedules the official serverless client's `close()` when a remote connection drops.

**Came up because:** Grok and Fable identified that the new SDK defers transaction rollback until connection reuse or stream closure. Temper opens a connection per store operation, so returning an error can drop both the transaction and connection; without Close, its server write lock remains until stream expiry. The previous driver scheduled Close on drop.

**Options:** Add explicit cleanup to every store return path; maintain an engine/client fork; restore connection ownership cleanup in the private adapter.

**Chose adapter cleanup over caller-wide changes or a fork because:** It covers success, early errors and cancelled operations at the resource boundary and uses the SDK's public Close operation. Cleanup runs on the existing Tokio runtime, matching the previous driver; shutdown without a runtime is reported rather than starting a separate runtime.

**Where:** `crates/temper-store-turso/src/driver.rs`; the protocol-level close regression in `src/driver/tests.rs`; PR https://github.com/nerdsane/temper/pull/457.

## D48 — Preserve retry classification for remote database contention

**Decision:** Preserve the serverless SDK's typed Busy and BusySnapshot errors as a distinct private driver error whose stable text is recognized by the existing store retry boundary.

**Came up because:** Codex showed that transparent error formatting removed the old Hrana stream-error marker. A hosted SQLITE_BUSY response then bypassed the existing retry budget even though retrying the complete operation is valid.

**Options:** Match generic lock-message text for every backend; change the shared kernel persistence error API; retain the remote SDK's typed classification at the adapter boundary.

**Chose typed remote classification because:** It restores remote contention retries without changing local-error behavior or treating constraints and read-only failures as retryable. The public persistence error contract remains unchanged; its existing string boundary receives an explicit remote-busy marker.

**Where:** `crates/temper-store-turso/src/driver.rs`; `src/retry.rs`; `src/driver/tests.rs`; PR https://github.com/nerdsane/temper/pull/457.

**D48 follow-up:** Fable also identified loss of network-error detail. The base libSQL sender uses Hyper's Display, which includes its source; the new SDK's request and cursor-stream errors retain only flattened messages. Classify those two transport-failure forms at the private adapter too, while leaving HTTP status failures and malformed responses non-transient. The pinned SDK has no finer transport cause available. Retrying a transport failure within the existing budget preserves recovery from resets; it can also retry another connection failure whose finer cause the SDK discarded. No new retry loop, budget, HTTP client or upstream patch is added.


## D49 — Preserve the local connection lock-wait default

**Decision:** Set the official Turso connection busy timeout to five seconds when each local connection opens.

**Came up because:** Fable found that the previous driver set this timeout on every connection while the new engine defaults to immediate Busy. Plain store connections, including OTS and tenant writes, do not apply the longer configured-writer timeout. The contended-write regression failed before this correction.

**Options:** Add retries to individual callers; change write concurrency; restore the existing connection default through the public SDK.

**Chose the connection setting because:** It preserves all callers and the existing longer configured timeout without another retry loop or engine patch.

**Where:** `crates/temper-store-turso/src/driver.rs`; `src/driver/tests.rs`; PR https://github.com/nerdsane/temper/pull/457.

## D44: Report the reaction-rule count instead of asserting on it

**Decision:** `register_tenant_rules` warns when a tenant exceeds `MAX_REACTIONS_PER_TENANT` and registers every rule; the assertion that aborted is gone. `MAX_REACTION_DEPTH` stays a hard bound.

**Came up because:** Tenant `default` reached 265 reaction rules across fifteen apps. The assertion panicked, which surfaced as three unrelated-looking failures: every inline spec load returned an unexplained 502 in 0.2s, the twin's `CollectionMeasured` trigger was never registered so the dispatcher returned early and silently with no observation and no log line, and on restart the panic ran on the main thread while replaying specs already committed to disk, so the platform crash-looped and was recoverable only by deleting rows by hand.

**Options:** Raise the constant; exempt the startup replay path from the assertion; return an error rather than panicking; delete the constant outright; or keep it as an advisory threshold and report.

**Chose report over assert because:** The constant bounded nothing. It sized no buffer and terminated no loop — rules live in growable `BTreeMap`s, so rule 257 costs what rule 250 costs — and it appeared only in its definition, a re-export, the assertion, and a test asserting the assertion. Being tenant-wide it was also unownable: no app could be written to respect it, and each app installed tightened it for the rest. Raising it moves the same outage to a later tenant. Exempting startup splits one rule into two behaviours by caller and still rejects specs the platform has already accepted. Returning an error still refuses a legitimate installation to protect nothing. Deleting it loses the visibility into unbounded growth, which is the one thing it provided. `MAX_REACTION_DEPTH` is untouched: an unbounded reaction cascade has no natural stopping point and the bound is what terminates the loop, so that assertion earns its place.

**Where:** crates/temper-server/src/trigger/registry.rs:53-61; commit 3878ad82; nerdsane/temper#464.

**Review disposition (Greptile, two findings, both acted on):** The first — that a non-fatal budget contradicts the repository's "budgets not limits, fail fast on invariant violation" line — was correct as written, so the convention now states the distinction the code relies on (a budget asserts only where exceeding it corrupts something; a count over a growable structure is reported), recorded as ADR-0176 because it outlives this effort. The second — that the regression test's comment claimed every rule "still dispatches" while the test calls only `ReactionRegistry::lookup` — was also correct; the comment now says what is proven, that every rule is found by `lookup`, which is the single call the dispatcher makes to decide what fires and the stage that previously came up empty. Guard evaluation, authorization and target resolution run per rule afterwards and do not vary with the tenant's rule count; they are covered by the dispatcher's own tests.

## MCP setup: explicit human administration

**Decision:** Add an argument-free native MCP setup operation using the existing service administration API, rather than changing the self-approval guard or allowing execute code to set policy.

**Came up because:** Genesis has only an operator requester, so its real human approval fails self-resolution. Recovered TemperPaw setup history shows manual operator identity provisioning was the missing initial step. Rita approved making that human setup possible through chat.

**Options:** Reuse the operator on both sides; grant access from execute code; add a native human-only administration operation.

**Chose the native operation because:** The human sees the fixed service, tenant, and grant, while agent input cannot choose policies or provide consent. Existing governance continues after setup. This adds a second MCP tool and requires private per-service credential storage.

**Where:** docs/adrs/0177-human-authorized-mcp-identity-setup.md; crates/temper-mcp/src/setup.rs; setup_consent.rs; setup_identity.rs.


## MCP setup: fail closed during partial provisioning

**Decision:** After human consent and private persistence, finish the old audit and switch execution to the candidate requester before the first administration write.

**Came up because:** Review showed that a successful policy grant followed by failed or canceled identity provisioning would otherwise leave execute using the newly empowered operator, and successful setup retained the operator's audit attribution.

**Options:** Restore operator execution on failure; add a separate execution-block flag; replace requester credentials before provisioning and initialize a new audit.

**Chose credential replacement because:** The existing credential boundary fails closed even if provisioning is interrupted. The operator remains available only for native setup recovery and human approvals. An unregistered requester receives authentication failures until explicit recovery completes; it never falls back to operator execution.

**Where:** crates/temper-mcp/src/setup.rs; setup_test.rs; setup_server_test.rs; ADR-0177.
