# TypeSafe System One guards

An IOA action can combine ordinary deterministic conditions with TypeSafe
judgments inside its existing conjunctive guard list. Each `system_one` entry
contains TypeSafe's `model`, `state`, and `questions`, plus a Temper `assert`
condition over the returned answers.

```toml
[[action]]
name = "Escalate"
kind = "input"
from = ["Open"]
to = "Escalated"
guard = [
  { type = "is_true", var = "assigned" },
  { type = "system_one", model = "jev-latest", state = { ref = "entity.Messages" }, questions = { human_requested = { type = "noul", instructions = "Has the customer explicitly requested a human?" } }, assert = "answers.human_requested.noul >= 0.8" },
]
```

Every entry must pass. The state machine needs no additional evaluation state.
Temper evaluates the model before committing the requested transition and keeps
the actual guard comparison deterministic.

A System One action requires a unique action declaration. Its `from` list can
include several lifecycle states; duplicate declarations with the same action
name are rejected before deployment.

## Question types

Choice selects one named option. Its assertion can inspect `choice` and
`confidence`:

```toml
guard = [
  { type = "system_one", model = "jev-latest", state = { ref = "entity.Messages" }, questions = { department = { type = "choice", instructions = "Which team should handle this request?", criteria = { technical = "Bugs and product malfunctions", billing = "Charges and payment problems", other = "Other requests" } } }, assert = 'answers.department.choice == "technical" && answers.department.confidence >= 0.7' },
]
```

Score evaluates an ordered rubric beginning at zero. Its returned `score` can
fall between levels, and it also exposes `confidence`:

```toml
guard = [
  { type = "system_one", model = "jev-latest", state = { ref = "entity.Messages" }, questions = { urgency = { type = "score", instructions = "How urgently does this request need attention?", criteria = ["Work can continue normally", "Work is degraded but a workaround exists", "Work is blocked with no workaround"] } }, assert = "answers.urgency.score >= 1.5 && answers.urgency.confidence >= 0.7" },
]
```

Noul returns a `noul` probability between zero and one. It has no separate
confidence field. Assertions support typed scalar comparisons combined with
`&&`; arbitrary expressions and executable code are not supported. Decimal
comparisons use fixed-point normalization rather than platform-dependent
floating-point calculations.

One entry can contain several questions; all evaluate the same resolved state.
See the [TypeSafe API reference](https://docs.typesafe.ai/api) for the native
question and answer formats.

## State bindings

TypeSafe's `state` is the content to evaluate, which can be text, an object, or an
array. It is independent of the entity's lifecycle status. Temper resolves only
the fields explicitly selected by the specification:

```toml
state = { subject = { ref = "entity.Subject" }, conversation = { ref = "entity.Messages" }, latest_message = { ref = "params.Message" }, policy = "Escalate when the customer explicitly requests a human." }
```

`entity.<field>` reads the pre-transition entity snapshot. `params.<name>` reads
the attempted action's parameters. Objects and arrays recursively compose
references and literals. Reference objects must contain only the `ref` key.
Missing fields fail explicitly. Related entities are not read automatically,
and context is never silently truncated.
Resolved context is limited to 64 KiB of JSON, including escaping and container
overhead. Each selected value is counted before it is cloned, so repeated
references cannot allocate an oversized inference context. The complete outbound
request has its own 64 KiB limit, including the model and questions.

Entity bindings include the canonical `Id` and `Status`, declared counters,
booleans, lists, and persisted data fields. Referenced entity fields stored as
overflow blobs are hydrated within the context byte budget. Parameter values
and literals remain data. Deployment rejects references to undeclared fields or
action parameters before publishing a new spec.
Authorization attributes such as `has_spec` and `HasSpec` are not entity data
and cannot be selected as model context.

Temper sends the resolved values as TypeSafe's native `state`; binding objects
and the Temper assertion are not part of the outbound API request. See the
[TypeSafe state documentation](https://docs.typesafe.ai/concepts/state).

## Credentials and local serving

`temper serve` attaches the production TypeSafe provider when it constructs the
secrets vault. Provision a tenant-local secret named `TYPESAFE_API_KEY` using:

```text
PUT /api/tenants/{tenant}/secrets/TYPESAFE_API_KEY
Authorization: Bearer <Temper tenant credential>
Content-Type: application/json

{"value":"<TypeSafe API key>"}
```

The authenticated credential must belong to the path tenant and have Cedar
permission for `Action::"manage_secrets"` on
`Secret::"TYPESAFE_API_KEY"`. A bootstrap operator initially receives
`manage_policies`, which lets an authorized operator append an appropriate
secret-management policy through
`POST /api/tenants/{tenant}/policies/rules`; it does not automatically grant
secret management. The new permission should remain scoped to the intended
principal, tenant, and secret.

Executing a System One guard also requires Cedar permissions for
`Action::"http_call"` on `HttpEndpoint::"api.typesafe.ai"` and
`Action::"access_secret"` on `Secret::"TYPESAFE_API_KEY"`. These are evaluated
under the authenticated action caller, alongside the action's usual permission.
Provisioning a key alone does not grant permission to evaluate guards.

The existing secret endpoint encrypts the value before persisting it and updates
the vault cache after persistence succeeds. Keep `TEMPER_VAULT_KEY` stable across
server restarts so persisted secrets remain decryptable. Shared platform
credentials are excluded from System One evaluation; each tenant needs its own
explicit secret. Changing or deleting a secret takes effect on subsequent
evaluations.

For local verification, `.env.typesafe.local` may hold `TYPESAFE_API_KEY`, but
Temper does not automatically import that file or expose its key to every
tenant. A local test driver must load the ignored file and provision the isolated
test tenant through the secret endpoint. The secret never belongs in an IOA
specification, evaluation receipt, or log.

Use an isolated local database and a Temper tenant credential when serving:

```sh
unset TURSO_PLATFORM_URL
mkdir -p .scratch
TEMPER_API_KEY=local-verify \
TURSO_URL="file:$PWD/.scratch/system-one.db" \
  cargo run -p temper-cli -- serve --port 3600 --storage turso
```

Set a retained local `TEMPER_VAULT_KEY` before provisioning secrets. A local
Turso file requires no cloud database credential.

## Evaluation and replay

Temper records evaluation evidence before using it to authorize a transition.
Evidence is bound to the tenant, caller, action, parameters, entity revision,
and specification identity. Successful and unsuccessful judgments are retained;
a retry of the same logical attempt reuses its recorded outcome. Explicit
reevaluation requires a new attempt. Supply the HTTP `Idempotency-Key` header on
guarded action requests: reuse that key when retrying the same action with the
same parameters, and use a new key for deliberate reevaluation. Successful
transition events carry `system_one_receipts` references to the corresponding
evaluation journals. A deployment without durable storage refuses these guards.

An immutable attempt reservation also detects specification changes that remove
or reorder guards. Reusing a key with a different caller, action, parameters, or
specification produces a conflict. A completed retry may return its committed
result after the entity advances; an incomplete attempt requires the original
entity revision. Guard assertions that fail return a conflict without a domain
transition. Provider errors are recorded and require a new attempt to reevaluate.

If the entity or spec changes while inference is in progress, the old answer
cannot enable a transition. Provider failures, malformed responses, missing
evidence, and stale evidence leave domain state unchanged. Replay uses durable
evidence and committed transition events without calling TypeSafe.

The production HTTP adapter uses the fixed
`https://api.typesafe.ai/v1/systemone` endpoint, rejects redirects, and allows at
most eight concurrent evaluations. Serialized requests are bounded to 64 KiB,
response bodies to 128 KiB, and total evaluation duration to 15 seconds, including
body reads. Saturation fails immediately instead of forming an unbounded queue.
The adapter does not automatically retry model calls or include upstream error
bodies in client errors.

This version executes System One guards through Temper's native Rust entity
actors. Composite execution and the separate Postgres actor adapter explicitly
refuse guarded actions. Postgres remains usable as the native actors' event store.

## Verification scope

Temper treats model answers as external inputs. Safety verification explores
possible guard outcomes, and deterministic simulation exercises the production
execution path with an injected provider. These checks establish that declared
guards and invariants are enforced for the modeled inputs. They do not establish
the accuracy of TypeSafe's judgments. Liveness can depend on acceptable answers
and provider availability.

The architecture is specified in
[ADR-0178](adrs/0178-system-one-guards.md).
