# Recorded System One guard contract

A System One entry lives in the existing typed IOA guard list. Its `model`,
`state`, and named `questions` follow Jev's request structure. Questions support
Choice, Score, and Noul. Temper adds `assert`, a typed conjunction of scalar
comparisons over `answers`. Existing guard entries remain conjunctive. Static
validation rejects malformed questions, invalid assertion paths, undeclared
state bindings, and unsupported question/answer shapes.

`state` consists of literal values, arrays, objects, and recursive
`{ ref = "entity.Field" }` / `{ ref = "params.Parameter" }` bindings. Entity
bindings read the authoritative pre-transition state, including selected
overflow-backed fields; parameters come from the validated action request.
Missing values, authorization metadata used as entity data, and oversized
context are errors. Context assembly does not execute expressions or read
related entities.

The provider receives only the resolved native request and the tenant's vault
secret `TYPESAFE_API_KEY`. It uses the fixed Typesafe HTTPS endpoint, rejects
redirects, bounds request/response sizes and concurrency, and applies network
deadlines. Errors redact credentials and response bodies. The caller must pass
action, outbound-HTTP, and secret-access authorization before inference and
again before its result can enable an action.

## Logical attempt and receipt state

| Input | Required condition | Result |
| --- | --- | --- |
| New logical attempt | Deterministic checks, admission and authorization pass | Reserve immutable request identity |
| Model response | Native answer schema validates | Durably record enabled or refused assertion outcome |
| Model/provider failure | Attempt exists | Durably record failure; no domain effects |
| Duplicate attempt | Tenant, principal, action, parameters and specification match | Reuse durable result; no resampling |
| Duplicate key with changed identity | Any bound request input differs | Conflict; no provider call or effects |
| Guarded transition | Recorded positive evidence and exact current precondition/specification match | Apply effects and append receipt references |
| State/specification changes during inference or OCC catch-up | Evidence no longer matches | Refuse transition |
| Restart/replay | Committed transition and referenced evidence exist | Restore state without inference |

The durable attempt binds tenant, principal, entity, action, parameters,
precondition, and full specification identity independently of guard order.
Receipt append uses first-writer-wins semantics. A crash before recording may
repeat inference, but no unrecorded response may commit a domain transition.
Completed retries allow the entity to have advanced because of their already
committed action while still checking request identity.

Invariants: domain effects require validated positive recorded evidence;
negative, failed, missing, forged, stale, or cross-tenant evidence never enables
an action; a reused attempt never silently changes its caller, request, or
specification; replay never invokes the provider; secrets never enter the
request specification, receipt journal, or logs. Only kernel code can construct
trusted actor evidence.

The shared parser and translator carry the full guard to runtime and the
verification cascade. Verification treats answers as nondeterministic external
inputs and preserves consistency within one typed response. Safety results do
not establish model accuracy; reported liveness assumptions and approximation
boundaries remain explicit.

V1 executes on native Rust actors with durable event storage. Composite paths
and the separate Postgres actor adapter refuse System One actions before any
domain write. Native actors may still use the Postgres event store.
