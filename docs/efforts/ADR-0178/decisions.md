# Decisions and tradeoffs

## D1: Native System One request inside the existing guard list

**Decision:** Use `type = "system_one"` in `guard = [...]` with native `model`,
`state`, and `questions`, plus Temper's typed `assert` over the answers.

**Came up because:** The user wanted Choice, Score, and Noul to inform
preconditions rather than lifecycle states, requested Jev-shaped configuration,
and explicitly asked to avoid extending the guard syntax beyond list entries.

**Options:** Add model-specific states; introduce a separate judgment declaration
and named references; place the request in a typed guard entry.

**Chose the typed entry because:** It keeps ordinary conjunctive guard behavior
and presents the API request at the action that depends on it. Assertions remain
small typed comparisons instead of an arbitrary expression runtime.

**Where:** `temper-spec::automaton::system_one`, the typed guard parser,
`temper-jit::table`, and `docs/system-one-guards.md`.

## D2: Explicit state selection from authoritative entity data and parameters

**Decision:** Resolve recursive entity/parameter references and literals from the
validated pre-transition context, hydrating only selected overflow-backed data.

**Came up because:** Jev requires `state`, and the model must judge the actual
data the transition is about to use without receiving unrelated tenant state or
authorization internals.

**Options:** Send the whole entity or tenant context; let an integration assemble
arbitrary state; use declared explicit bindings in the guard.

**Chose explicit bindings because:** The specification defines its model inputs,
static validation catches missing names, and the runtime can bind the result to
the exact precondition. Related-entity reads and expression execution remain
outside V1.

**Where:** `crates/temper-server/src/system_one/resolve.rs`, registry validation,
and `crates/temper-spec/src/automaton/system_one/state.rs`.

## D3: Durable outcomes for immutable logical attempts

**Decision:** Reserve request identity and record validated positive, negative,
and failed outcomes before actor evidence can enable a domain transition.
Retries reuse the same outcome; explicit reevaluation uses a new attempt key.

**Came up because:** Inference is external input. Resampling a refused retry or
using an unrecorded response would change the meaning of an action across
concurrency, faults, restart, or hot specification deployment.

**Options:** Evaluate inside every guard check; cache model answers only in
memory; durably bind attempts and receipts to caller, request, precondition, and
specification.

**Chose durable binding because:** Replay needs no provider, duplicate requests
cannot change their identity, and actor/OCC checks reject stale evaluations.
Reservation is independent of guard order so removing or reordering guards
does not make an old key fresh. A crash before the receipt may repeat inference
but cannot commit an unrecorded domain transition.

**Where:** `crates/temper-server/src/system_one/{attempts,receipts,evidence}.rs`,
guarded actor dispatch, and committed transition events.

## D4: Tenant vault credentials and the normal caller authorization boundary

**Decision:** Read `TYPESAFE_API_KEY` from the executing tenant's vault and require
action, outbound-HTTP, and secret-access permissions before and after inference.
Keep HTTP in an injected provider with fixed endpoint and bounded resources.

**Came up because:** A judgment call spends a tenant credential and exposes
selected context outside the kernel. A global environment fallback would bypass
tenant ownership and make simulated execution depend on ambient process state.

**Options:** Read a process-global API key; let clients supply keys per action;
use the existing vault and Cedar permissions with an injected provider.

**Chose the vault and provider boundary because:** Credentials remain tenant
scoped, unauthorized actions do not make paid calls, provider errors redact
sensitive data, and simulation uses the same production receipt/guard logic.

**Where:** `crates/temper-server/src/system_one/provider.rs`, dispatch context
resolution, and `crates/temper-server/src/secrets/vault.rs`.

## D5: Explicit first-release execution and verification boundaries

**Decision:** Execute System One guards on native Rust actors and refuse them on
composite paths and the separate Postgres actor adapter. Verification explores
typed external answers and reports assumptions instead of claiming model
accuracy or unsupported execution coverage.

**Came up because:** A new guard must not become implicitly true on a runtime
path that lacks durable evidence and freshness checks; probabilistic model
output is not a deterministic correctness proof.

**Options:** Enable every execution adapter immediately; accept ordinary guard
booleans on unsupported paths; support one enforcing path and reject the rest.

**Chose explicit boundaries because:** No unsupported path can write before
refusal, and the existing native actor event-store abstraction still supports
Postgres storage. The full guard reaches the cascade, which separates safety
from model-accuracy and liveness assumptions.

**Where:** `temper-actor-runtime::spec_actor_system_one`, composite/native
dispatch, `temper-verify`, and ADR-0178.
