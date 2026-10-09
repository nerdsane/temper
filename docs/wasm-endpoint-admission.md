# Authenticated actions before WASM HTTP execution

An HttpEndpoint may declare `AdmissionActions`, a JSON-encoded array of ordered
OData bound actions. Before starting the WASM module the kernel runs each action
through the ordinary OData handler with the incoming request's authenticated user.
Cedar authorization, input validation and IOA state checks therefore apply before
external I/O. A rejection is returned directly; the module is not started.
Admission targets must already exist: route declaration and an initial-state action
do not imply creation. An authorized missing or deleted target returns
`404 EntityNotFound`; Cedar denial still precedes existence disclosure. This rule
also applies to native endpoint admission and guest calls to OData bound actions.

Example field value:

```json
[{"name":"target","entity_set":"Targets","entity_id":"{id}","action":"Example.Serve"}]
```

The endpoint path supplies `{id}`. Captures are decoded once and escaped as OData
string values. Optional `params` maps parameter names to literals or entire capture
placeholders. The module receives successful action responses as `entity_state`,
keyed by `name`. These are request-local inputs, not new persisted entity fields.
No caller credential is passed to the module. Checks run on every request even
when an incoming idempotency key repeats. Ordered actions are not a transaction;
an earlier successful action is not rolled back if a later action rejects.

Before any action runs, the kernel resolves the WASM module (or native transport)
and validates the complete action declaration: unique response names, schema
identifier syntax, capture values and OData paths. Native requests are bounded to
8 MiB before actions execute. Validation failures do not execute earlier actions.
An action that passes validation but later fails authorization or its IOA checks
still leaves earlier successful actions committed.

Admission requires a completed action result. PostgreSQL-backed actions currently
return HTTP 202 when queued, before IOA execution completes. Such admission targets
are rejected with HTTP 503 `AdmissionRequiresCompletion` before any action is
queued or executed. Native and WASM endpoints use the same rule; the kernel does
not treat queue acceptance as permission to perform external I/O.

`TimeoutSecs` supplies one deadline across admission and dispatch, without restarting
it when waiting for response headers. Native response bodies retain that deadline
while streaming and enforce `MaxResponseBytes` (1 MiB by default). If a limit is
exceeded after headers have been sent, the body ends with an error. Native transports
receive headers with the same caller-credential exclusions as ordinary WASM HTTP
endpoints, in addition to connection and identity-header filtering.

Endpoints without admission actions keep their existing behavior. Native transport
configuration and a separate admission list cannot be combined: malformed or
ambiguous configuration is rejected when constructing the route table.

## User and integration identity

Action-triggered integrations retain the caller's identity for local OData calls.
The kernel additionally supplies `context.module` naming the actual executing
module, overwriting any inherited value. This matches existing internal HTTP
credential behavior. It lets Cedar require both user ownership and the specific
integration. Direct WASM execution also supplies the module identity on local calls.

WASM HTTP endpoint modules continue making their own internal calls as the module;
the new pre-execution actions run as the incoming user. Existing protocol handlers
therefore retain their own module permissions.

Secret access remains separately authorized as the module, with the triggering
user available in the authorization context. Alice and Bob can trigger the same
integration using one webhook secret, without giving either user direct access
to that secret or allowing the integration to read unrelated secrets.

The compiled fixture in `crates/temper-server/tests/fixtures/wasm-identity` and
`wasm_identity_admission` tests exercise these boundaries with real WASM, Cedar,
OData and libSQL. The change contains no application-specific lifecycle rules.

## Standard-server reload regression

A real control-plane update exposed a separate startup defect. `serve --app`
restored passed verification from libSQL, loaded the same disk spec with a Pending
status, then skipped its background verification because the hash was cached as
verified. Reloading identical passed specs now retains the original evidence.
Changed, failed, running and pending specs still require verification.

Four CLI regressions cover this behavior. Before the fix, the two preservation
cases failed; all four pass afterwards. A real standard-server process started
twice against one libSQL database changed from `passed, pending` to `passed, passed`.
The full CLI suite passed 78 tests. This fixes normal application replacement; it
does not add control-plane recovery for interrupted integrations.
