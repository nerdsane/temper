# ADR-0187: Versioned Application Failure Envelopes

- Status: Proposed
- Date: 2026-08-26
- Deciders: Temper core maintainers
- Related:
  - ADR-0046: Unified Action Triggers
  - ADR-0048: Dispatch Retry and Error Taxonomy
  - ADR-0152: Fail-Closed WASM Trigger Outcomes
  - ADR-0157: Metadata-Generated Typed Module Data SDK
  - ADR-0158: Durable Observable Entity Reactions
  - ADR-0178: Durable State Timeout Delivery
  - `crates/temper-spec/src/automaton/types.rs`
  - `crates/temper-server/src/state/dispatch/`
  - `crates/temper-wasm-sdk/src/data/contracts.rs`

## Context

Temper now exposes structured module-data errors with stable kinds, codes,
retry guidance, optional governance decision identity, and bounded metadata.
That contract stops at the module-data ABI. Application triggers still select a
single `on_failure` callback and pass free-form `error` and `error_message`
strings. Entity specifications therefore cannot distinguish a transient outage
from an integrity violation, authorization denial, exhausted budget, ambiguous
commit, or permanent failure without application code parsing diagnostics.

The same gap exists at other kernel boundaries. Durable reactions record an
optional string, dispatch timeouts collapse to display text, authorization
denials are recognized in some WASM paths by searching an error string, and
external-operation errors arrive through several runtime-specific types. These
paths lose causal identity and make retry or operator routing implicit. A message
wording change can consequently alter control flow.

The contract must work in specification parsing, generated guest SDKs,
verification, JIT metadata, runtime dispatch, and deterministic simulation. It
must not make `temper-spec` depend on a guest SDK or make the SDK depend on the
spec parser and verifier.

## Decision

Introduce one dependency-light `temper-failure` crate containing the canonical
v1 application failure contract. `temper-spec`, `temper-jit`, `temper-verify`,
`temper-server`, `temper-wasm`, `temper-wasm-sdk`, and `temper-codegen` consume
that contract at their existing boundaries. The crate contains data contracts,
bounds, validation, and source-neutral constructors; adapters for a crate-local
error remain in the crate that owns that error.

### Sub-Decision 1: Use A Closed, Versioned Envelope

The first wire contract is `FailureEnvelopeV1`:

```text
FailureEnvelopeV1 {
    version: 1,
    category: transient | integrity | authorization | budget | ambiguous | permanent,
    code: StableFailureCode,
    retryability: never | after_refresh | with_backoff | after_authorization | reconcile,
    outcome: not_applied | applied | unknown,
    operation: CausalOperationV1 { id, kind, attempt, parent_id? },
    provenance: FailureProvenanceV1 { source, component, source_code? },
    message?: BoundedDiagnostic,
    diagnostic_omitted: bool,
    details: BoundedFailureDetails,
    details_omitted: bool,
}
```

`version` is serialized and must equal one. Unknown fields and enum values are
rejected. Every variable-length field uses these v1 budgets:

| Field | V1 budget |
|---|---:|
| stable failure code | 64 UTF-8 bytes |
| operation ID or parent operation ID | 128 UTF-8 bytes each |
| operation kind | 64 UTF-8 bytes |
| provenance component or source code | 64 UTF-8 bytes each |
| diagnostic message | 512 UTF-8 bytes |
| detail entries | 16 |
| detail key | 64 UTF-8 bytes |
| detail string value | 256 UTF-8 bytes |
| complete serialized details object | 2,048 bytes |
| operation attempt | integer from 0 through 1,024 |

Stable codes, operation identifiers and kinds, provenance components and source
codes, and detail keys are non-empty ASCII tokens containing only letters,
digits, `.`, `_`, `:`, or `-`. Provenance `source` is the closed enum
`module_data | reaction | timeout | authorization | external_operation | wasm |
legacy`; it is not a variable string. Detail values are limited to string,
signed integer, unsigned integer, and boolean scalars in a `BTreeMap`; nested
JSON, floating point, null, arrays, and objects are excluded from v1.

Public constructors and custom deserialization validate every budget. Invalid
wire values return `FailureContractError`; they are never truncated, hashed into
a replacement, or reclassified. Kernel-owned adapters build allowlisted details
through the same fallible API. If an optional diagnostic or detail from an
upstream system does not fit, the adapter omits the complete optional field and
sets the typed `diagnostic_omitted` or `details_omitted` flag. A bounded,
allowlisted subset of safe details may remain when other upstream details are
omitted; `details_omitted` records that the map is incomplete. It never keeps a
text prefix whose meaning may change. The required category, code, retryability,
outcome, operation, and provenance must validate or the adapter itself fails
closed with the bounded permanent code `InvalidFailureAdapterOutput`.

Diagnostics are never exposed to routing APIs. Exact constants live in
`temper-failure` and changing any one requires a new envelope version.

`outcome` is independent of category. An authorization failure is normally
`not_applied`; an acknowledgement timeout may be `unknown`; a post-commit
projection failure may be `applied`. The `ambiguous` category is used when
unknown application outcome is the primary routing fact, while the outcome field
preserves that fact for every other category and adapter.

**Why this approach**: versioning and closed enums make compatibility explicit.
Separate category, retryability, and outcome fields prevent one overloaded enum
from hiding whether work committed or which recovery is safe.

### Sub-Decision 2: Route Failures At The Trigger Boundary

Typed routes are declared on the trigger that can fail:

```toml
[[action.triggers]]
name = "charge_card"
kind = "wasm"
module = "payments"
on_success = "ChargeSucceeded"

[[action.triggers.failure_routes]]
category = "transient"
action = "RetryCharge"

[[action.triggers.failure_routes]]
category = "authorization"
to_state = "AwaitingApproval"

[[action.triggers.failure_routes]]
category = "ambiguous"
action = "ReconcileCharge"
```

Each category may appear at most once per trigger. A route declares exactly one
of `action` or `to_state`. An action must exist on the source automaton. A state
shorthand is accepted only when verification finds exactly one source-entity
action enabled from the trigger source action's committed state and targeting
that state; the resolved action name is stored in production metadata. Zero or
multiple candidates reject the specification. The runtime never mutates status
directly on a failure path.

When an envelope category has no declared route, dispatch returns
`UndeclaredFailureCategory` and records the envelope for observation. It does
not choose a broad `Blocked` or `Invalid` state, invoke another route, or infer a
route from the message. Route callbacks receive the serialized typed envelope
under `failure`; diagnostic compatibility fields are not added to typed routes.

Every resolved callback action must declare exactly one parameter in v1:

```toml
params = [{ name = "failure", type = "failure_v1" }]
```

The parameter name and type are canonical and case-sensitive. A routed callback
with a missing, renamed, differently typed, or additional parameter is rejected
during parse/verification. The same rule applies after `to_state` shorthand is
resolved. This avoids invoking an action without values for its required
parameters and gives code generation one exact callback ABI. `failure_v1` is a
kernel parameter type backed by `FailureEnvelopeV1`; ordinary callers cannot
substitute a string or unvalidated JSON object.

**Why this approach**: the trigger owns the fallible side effect and its recovery
policy. Resolving state shorthand to an ordinary action preserves Cedar checks,
audit events, guards, and deterministic transition semantics.

### Sub-Decision 3: Keep Legacy Free-Form Behavior Explicit

Existing `on_failure` remains supported for specifications that declare no
`failure_routes` on that trigger. It retains its current `error`,
`error_message`, and integration fields and is labeled as the legacy callback in
metadata and telemetry. A trigger that declares both `on_failure` and any typed
route is invalid.

Legacy errors are not parsed or silently mapped to a typed category. When a
legacy source reaches a typed-only runtime boundary, the adapter creates a
`permanent/LegacyFreeFormFailure` envelope with provenance marked `legacy` and
the original text only in the bounded diagnostic field. Such an envelope still
requires an explicit permanent route.

**Why this approach**: existing applications keep their exact behavior while
new behavior cannot accidentally depend on old strings. Authors migrate by
declaring routes and changing callbacks to accept `failure`, not through a
kernel heuristic.

### Sub-Decision 4: Define Source-Owned Typed Adapters

Adapters classify from structured variants and execution facts only:

- `ModuleDataError` maps kinds and its existing retryability directly, retains
  `decision_id` as a safe detail, and records the module-data operation ID.
- reaction delivery uses the durable delivery/intent ID. A rejected target
  transition is integrity; exhausted pre-dispatch capacity is transient; a
  lost acknowledgement after possible commit is ambiguous with unknown outcome.
- actor or state-timeout delivery records the durable timeout identity. An ask
  timeout after dispatch is ambiguous; a timer admission budget is budget; a
  cancelled stale generation is a known-not-applied integrity result.
- Cedar denial is authorization with `after_authorization`, decision identity,
  and known-not-applied outcome.
- WASM and external operations map typed engine, transport, HTTP-status, and host
  capability variants. Pre-execution payload and admission ceilings are budget.
  Fuel, memory, timeout, and invocation failures after guest execution begins
  are ambiguous because the guest may already have produced external effects;
  their stable source codes retain which budget or engine condition ended the
  invocation. Typed dependency unavailability is transient; an externally
  dispatched call whose acknowledgement is lost is ambiguous; malformed
  contracts and terminal responses are integrity or permanent as declared by
  the adapter table.

Adapter tests enumerate every source enum variant. Adding a new source variant
therefore requires an explicit mapping. No adapter calls `to_string()` and then
inspects the result to choose category, retryability, outcome, or code.

**Why this approach**: source crates know whether an operation was dispatched or
committed. Central string conversion would discard exactly the provenance and
ambiguity facts the envelope exists to retain.

### Sub-Decision 5: V1 Guidance Never Replays An Operation

The envelope does not retry, and v1 failure routes cannot request automatic
operation replay. Existing inner retry mechanisms retain their current bounded
scope: actor delivery, reaction delivery, timeout delivery, module-data
consistency waits, and transport attempts consume their own declared budgets
before their owning adapter emits exactly one final envelope. Envelope
retryability reports what a later governed operation may safely do; it does not
extend, reset, or create another attempt budget.

A routed callback may transition application state to an explicit recovery state
such as `RetryScheduled`, `AwaitingApproval`, or `Reconciling`. Any later retry is
a new ordinary action/trigger execution with its own causal operation ID and
declared delivery budget. For an `unknown` outcome, the kernel never repeats the
failed mutation automatically; it only dispatches the declared category route.
Whether that ordinary routed action performs domain-appropriate reconciliation
remains visible specification behavior subject to normal review and composite
verification—the kernel does not infer intent from an action name. Authorization
waits for a new Cedar decision or changed principal context rather than consuming
backoff attempts.

Retry-budget tests pin three invariants: inner attempts never exceed their
existing budget, exactly one envelope is emitted after exhaustion, and route
dispatch does not reset or consume the failed operation's budget. Deterministic
simulation uses the existing seeded scheduler for those inner attempts. Wall
clock, random UUIDs, unordered maps, and unbounded queues are forbidden.

**Why this approach**: guidance without a newly declared operation and budget
creates loops. Reusing the failed operation's budget after an envelope can also
duplicate externally committed work, especially when outcome is unknown.

### Sub-Decision 6: Generate And Redact The Typed Surface

`temper-wasm-sdk` re-exports `temper-failure` and replaces duplicated retry
types only through an explicit ABI-compatible conversion. Generated SDK clients
return the canonical envelope for application-facing failures and generate typed
callback parameter structs containing `failure: FailureEnvelopeV1` where a
route is declared. The IOA parameter token `failure_v1` maps to the canonical
CSDL type `Temper.FailureEnvelopeV1`; code generation recognizes that exact
type and imports the shared contract instead of emitting an entity-ID wrapper.

Telemetry records only version, category, code, retryability, outcome, bounded
operation identity, and allowlisted scalar details. Diagnostic messages and
non-allowlisted detail values are redacted by default. Stable code and category
are low-cardinality control fields; message text is never a metric label.

**Why this approach**: applications and operators see the same contract without
turning diagnostic or secret-bearing data into routing inputs or telemetry
cardinality.

## Rollout Plan

1. Add the shared v1 contract, bounds, serialization tests, and source adapters.
2. Add parser, verification, JIT metadata, and fail-closed route selection.
3. Dispatch typed WASM, reaction, timeout, authorization, and external failures;
   retain legacy callbacks only for route-free triggers.
4. Re-export and generate SDK surfaces, then enable typed routes in application
   specs through ordinary verified spec deployment.
5. Remove legacy callbacks only in a separately accepted compatibility ADR after
   usage telemetry shows no remaining declarations.

## Readiness Gates

- All envelope bounds and exact v1 encodings are pinned by tests.
- Parser, model checker, JIT, and runtime agree on every resolved route.
- Every required adapter enum is exhaustively classified without message input.
- Seeded DST covers transient exhaustion, unknown-outcome reconciliation,
  authorization waiting, and undeclared-category fail-closed behavior.
- Generated SDK golden and compile tests expose the exact `failure_v1` callback
  parameter contract.
- Telemetry and persistence tests prove diagnostics and unsafe details redact.

## Consequences

### Positive

- Application recovery policy becomes reviewable and verifiable specification
  data rather than string parsing.
- Kernel error sources retain causal and ambiguity information across adapters.
- Generated clients, runtime dispatch, and operators share one stable contract.
- Undeclared new failure classes stop safely instead of falling into a broad
  application state.

### Negative

- A new shared crate and explicit adapter tables add dependency and maintenance
  surface.
- Migrating a trigger requires new callback parameter types and routes for every
  category the operation may emit.
- State shorthand is intentionally strict and may require authors to name the
  callback action when multiple transitions share a destination.

### Risks

- Incorrect adapter classification could recommend unsafe retries. Exhaustive
  variant tests and unknown-outcome reconciliation mitigate this.
- Details may contain secrets despite scalar bounds. Construction allowlists and
  default-redacted telemetry mitigate this; bounds alone are not treated as
  sanitization.
- A broad permanent route could recreate generic Blocked states. Verification
  can prove declaration completeness, but domain review remains responsible for
  meaningful destination states.

### DST Compliance

- Contract maps use `BTreeMap`; exact serialization is deterministic.
- Causal IDs come from existing request, delivery, timeout, or
  `sim_uuid()`-derived identities; adapters do not generate OS randomness.
- Existing inner retries consume their bounded deterministic delivery budgets
  and simulated scheduler time; typed routes do not introduce another replay.
- Runtime and simulation execute the same resolved category-to-action metadata.

## Non-Goals

- Defining application-specific codes or states in kernel code.
- Parsing provider messages, exception strings, or HTTP response bodies to infer
  control flow.
- Automatically retrying unknown-outcome mutations.
- Automatically retrying any operation from envelope guidance in v1.
- Removing public OData error envelopes or legacy callbacks in this change.
- Moving TemperPaw, genesis, or katagami application policy into the kernel.

## Alternatives Considered

1. **Keep the contract in `temper-wasm-sdk`** — Rejected because spec, JIT, and
   non-WASM runtime paths would depend on a guest-facing SDK.
2. **Keep the contract in `temper-spec`** — Rejected because the generated guest
   SDK would pull parser and specification concerns into modules.
3. **Duplicate enums at each boundary** — Rejected because conversions drift and
   unknown categories can silently acquire different semantics.
4. **Route by stable code** — Rejected for v1 because codes are intentionally
   finer-grained and source-specific; category routes remain bounded and
   portable. Callbacks may still inspect a typed code diagnostically.
5. **Map directly to states** — Rejected because it bypasses ordinary action
   guards, Cedar authorization, audit events, and transition verification.
6. **Infer categories from legacy messages** — Rejected because diagnostic text
   is not a stable contract and can contain sensitive provider content.

## Rollback Policy

Typed route declarations are additive. Before application specs use them, the
shared crate and parser/runtime changes can be reverted together. After use,
rollback first redeploys affected specs with their explicit legacy `on_failure`
callbacks, then removes typed runtime support. Persisted envelopes retain their
version and remain readable as opaque audit data; they are never rewritten into
legacy strings.
