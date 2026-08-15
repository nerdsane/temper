# Scoped schema deployment foundations

This is the implementation handoff from backlog task 117 to task 114. ADR-0159
is authoritative. This document pins concrete integration seams and test
matrices so the continuation can implement mutable behavior without redesigning
the deterministic foundation or colliding with task 58.

## Shipped preparatory boundary

`temper-spec::ScopedSpecBundle::compile` is a pure compiler for the v1 schema
identity inputs:

- opaque task scope ID;
- optional canonical predecessor digest;
- typed IOA TOML keyed by fully qualified entity type; and
- parsed, sorted, canonically emitted CSDL;
- deterministically ordered Cedar and WASM descriptors;
- an optional versioned migration descriptor; and
- explicit positive verification and migration budgets.

The compiler rejects empty/over-budget IOA and artifact collections, malformed sources,
duplicate IOA keys, duplicate CSDL symbols, invalid predecessors, and IOA key /
automaton-name disagreement. It performs no registry, store, clock, random,
filesystem, or network work. Task 114 extends the digest envelope with the ADR's
policy, WASM, migration, and budget frames before making v1 externally writable;
the compiler remains side-effect-free until task 114 supplies durable lifecycle
and authorization adapters.

Task 117 deliberately does not add a temporary deployment store trait or touch
the live `SpecRegistry`. The real durable trait must be introduced with its
Postgres, Turso, and Sim implementations in one Red-Green change so its atomic
boundary cannot be weaker than its contract tests.

## Existing foundation inventory

### Task 61 / ADR-0157: shared governed service

- `crates/temper-server/src/application_data/service.rs` owns the
  transport-neutral governed entity operations. OData adapters and WASM host
  calls already converge here.
- `crates/temper-server/src/application_data/invocation.rs` captures tenant,
  module/artifact/grant identity, original `SecurityContext`, and bounded
  invocation resources on the host side. Guests cannot choose them.
- `crates/temper-server/src/application_data/authority.rs` validates the locked
  module binding and applies capability checks before delegating.
- `crates/temper-server/src/application_data/parity_tests.rs` is the pattern for
  proving HTTP/WASM semantic parity without making one adapter call the other.
- `crates/temper-wasm-sdk/src/data/` owns stable versioned request, response,
  error, grant, and budget encoding conventions.

Task 114 should add a sibling `schema_deployment` module and one
`GovernedSchemaDeploymentService`. HTTP and WASM adapters must call it directly;
neither adapter may call `PlatformStore`, a backend, or `SpecRegistry`.

### Task 95 / ADR-0158: durable worker machinery

- `crates/temper-server/src/trigger/delivery.rs` defines immutable intent,
  mutable lifecycle, stable SHA-256 identity, lease/fence accounting, receipts,
  bounded retries, and redaction inputs.
- `crates/temper-server/src/trigger/dispatcher/durable.rs` provides bounded
  materialization, claim, receipt reconciliation, recovery cursors, and wakeup
  calculations.
- `crates/temper-server/src/state/mod.rs` runs a per-tenant due-time supervisor
  with generation fencing, weak lifetime ownership, bounded one-batch work,
  independent deadlines, wakeups, pacing, and error backoff.
- `crates/temper-runtime/src/persistence/mod.rs` is the semantic store contract;
  its explicit transaction methods and bounded reads are the precedent.

Task 114 should reuse the intent/receipt/lease/fence/cursor pattern, but schema
deployment records remain a distinct domain. Do not encode deployments as fake
reaction deliveries or store migration payloads in reaction journals.

### Task 58-owned seams

Task 114 must consume, not recreate, these task-58 outputs:

- immutable typed-reference metadata and bundle cross-validation;
- schema digest pins on entity creation and every actor snapshot/event;
- registry-version pins on normalized reaction intents and receipts;
- actor eviction/rehydration after fenced cutover; and
- deterministic hot-reload audit over existing entities.

The integration must fail closed when any required pin is absent. It must not
infer a pin from whichever registry entry is active during recovery.

## Transport-neutral contract

All v1 envelopes use `deny_unknown_fields`, snake-case enum tags, explicit
positive budgets, and stable string codes. The logical shapes are:

```text
SubmitBundleRequestV1 {
  request_id, idempotency_key, scope { kind: task, id },
  expected_predecessor?, canonicalization_version,
  csdl, ioa[], cedar_policies[], wasm_modules[], migration?, budgets
}

VerifyBundleRequestV1 { request_id, idempotency_key, scope, bundle_digest }
ActivateBundleRequestV1 {
  request_id, idempotency_key, scope, bundle_digest,
  expected_predecessor?, expected_fence
}
RetireBundleRequestV1 {
  request_id, idempotency_key, scope, bundle_digest, expected_fence
}
StartMigrationRequestV1 {
  request_id, idempotency_key, scope, source_digest, target_digest,
  verification_receipt_id, expected_fence, budgets
}

DeploymentReceiptV1 {
  request_id, scope, bundle_digest, predecessor?, status, fence,
  verification_receipt_id?, migration_receipt_id?, committed_sequence
}
```

The status enum is exactly `submitted`, `verifying`, `verified`, `activating`,
`active`, `retiring`, `retired`, `rejected`. Adapters map stable service errors
without inspecting human messages:

| Code | HTTP | Retryable | Meaning |
|---|---:|---|---|
| `invalid_bundle` | 400 | no | Syntax or closed-enum failure |
| `duplicate_symbol` | 409 | no | Duplicate name/signature in immutable input |
| `scope_mismatch` | 409 | no | Request and stored scope differ |
| `digest_mismatch` | 409 | no | Claimed and computed content differ |
| `predecessor_mismatch` | 409 | after reread | Active pointer changed |
| `idempotency_conflict` | 409 | no | Same key, different canonical request |
| `verification_failed` | 422 | no | Verification receipt is terminal failure |
| `authorization_denied` | 403 | only after policy change | Cedar denied exact operation |
| `invalid_lifecycle_transition` | 409 | after reread | State does not enable operation |
| `stale_fence` | 409 | after reread | Worker/request lost ownership |
| `migration_budget_exhausted` | 422 | with new bundle/request | Declared budget consumed |
| `migration_rejected` | 422 | no | Module/fixture/determinism validation failed |
| `migration_failed` | 500 | bounded worker retry | Execution failed transiently |
| `backend_unavailable` | 503 | yes | No authoritative transaction completed |

### Cedar operations

Use the exact actions from ADR-0159. The resource UID must contain tenant,
scope kind, and scope ID. Bundle digest, predecessor, lifecycle state, and
whether private artifacts are requested are Cedar context attributes, not
principal-controlled headers. `get` returns redacted metadata unless a separate
private-artifact read is explicitly authorized. Retry preserves the original
accepted authority.

## Durable record contract

The shared store trait should expose semantic transactions rather than CRUD:

```text
submit_bundle(bundle, idempotency) -> SubmitOutcome
claim_verification(scope, digest, lease, expected_fence) -> VerificationClaim
finish_verification(claim, receipt) -> DeploymentRecord
compare_and_activate(scope, digest, predecessor, receipt, fence) -> ActivePointer
begin_retirement(scope, digest, expected_fence) -> DeploymentRecord
create_migration(job, idempotency) -> MigrationJob
claim_migration_batch(job, cursor, lease, expected_fence) -> BatchClaim
commit_migration_batch(claim, rows, next_cursor, receipt, budgets) -> MigrationJob
compare_and_cut_over(job, validation_receipt, expected_fence) -> ActivePointer
page_deployments(scope, after, budget) -> Page
page_migration_source(job, after, budget) -> Page
```

Required records and keys:

| Record | Primary identity | Immutable fields | Mutable fenced fields |
|---|---|---|---|
| bundle | tenant, scope, digest | canonical inputs/artifact digests | none |
| deployment | tenant, scope, digest | predecessor, submit authority | status, fence, lease |
| active pointer | tenant, scope | none | digest, predecessor, fence |
| idempotency | tenant, operation, key | canonical request digest | receipt reference |
| verification receipt | tenant, scope, digest, verifier ABI | all | none |
| migration job | tenant, scope, target digest | source digest, module digest | state, fence, cursor, budgets |
| batch receipt | job, source cursor | input/output digests, row count | none |
| schema pin | tenant, entity identity, sequence | bundle digest | none |

Every list/page method accepts a positive item budget and returns a stable
keyset cursor plus `exhausted`. Offset scans and unbounded tenant enumeration are
not permitted.

### Backend contract scenarios

Run each scenario unchanged against Postgres, Turso, and Sim:

1. identical submit/key returns the original receipt;
2. different canonical input under the same key conflicts without a write;
3. duplicate concurrent submit stores one immutable bundle;
4. verification claim expiry increments the fence and rejects the old worker;
5. failed verification never creates/changes the active pointer;
6. activation succeeds only for the expected predecessor, receipt, and fence;
7. concurrent activation yields one winner and one stable conflict;
8. every reader during activation sees the complete old or new digest;
9. migration batch commits rows, cursor, budget use, and receipt atomically;
10. replay of a committed batch is a no-op with the original receipt;
11. mismatched replay digest rejects without changing shadow state;
12. cutover requires complete scan, caught-up journal, and validation receipt;
13. pre-cutover crash resumes/abandons shadow state without reader impact;
14. post-cutover crash recovers forward without restoring the old pointer;
15. retirement blocks new pins and preserves existing pinned reads;
16. keyset pages are stable across interleaved inserts and tombstones;
17. tenant and scope isolation hold for every lookup and transaction.

## Pure migration WASM ABI

The only v1 export is `temper_schema_migrate_v1`. Its encoded input/output
contract is:

```text
MigrationInputV1 {
  abi_version: 1,
  source_bundle_digest,
  target_bundle_digest,
  entity_type,
  entity_id,
  source_sequence,
  canonical_state_json,
  logical_context { batch_id, item_index }
}

MigrationOutputV1 =
  { outcome: unchanged } |
  { outcome: replace, canonical_state_json } |
  { outcome: reject, code, message }
```

The module has no WASI or Temper host imports. Specifically reject filesystem,
socket/network, environment, clock, random, thread, host HTTP, application-data,
file-stream, actor-dispatch, registry, and secret imports. Allow only linear
memory plus the agreed allocation/deallocation ABI. Validate imports before any
fixture execution.

Budgets are consumed and durably recorded: fuel per item, memory pages, input
bytes, output bytes, JSON depth/nodes/string bytes, entities per batch, total
entities, batches, attempts, and logical deadline. A zero or platform-exceeding
budget is invalid input. Traps, malformed UTF-8/JSON, non-finite numbers,
undeclared fields, wrong target types, and oversized output are stable rejects.

Verification invokes every sanitized vector twice and requires byte-identical
canonical output. Runtime replay additionally binds source cursor, input digest,
output digest, module digest, and target bundle digest.

### Negative fixture corpus

- unknown import and each forbidden import family;
- missing/wrong export signature and ABI version;
- trap, fuel exhaustion, memory growth exhaustion, infinite loop;
- malformed UTF-8, malformed JSON, duplicate JSON keys, excessive nesting;
- NaN/infinity encodings and non-canonical numbers;
- output for the wrong entity type or undeclared property;
- output above byte/node/string budgets;
- nondeterministic repeated output;
- input/output digest mismatch on replay;
- reject message above the bounded redaction limit;
- attempted cross-entity fan-out.

Fixtures contain invented entity IDs and values only. Never use production
tenant data, principal claims, policy text, secrets, or customer schemas.

## Crash and DST matrix

Inject a crash immediately before and after each durable boundary:

| Boundary | Pre-commit recovery | Post-commit recovery | Visibility invariant |
|---|---|---|---|
| bundle + idempotency insert | retry submit | return original receipt | no partial bundle |
| verification claim | reclaim after lease | old fence rejected | no duplicate finisher |
| verification receipt | rerun verification | reuse receipt | inactive until activation |
| activation CAS | old remains active | new remains active | old or new, never mixed |
| migration job create | retry start | return original job | source stays active |
| batch claim | reclaim after lease | old fence rejected | shadow hidden |
| transformed rows + cursor | replay batch | resume next cursor | shadow batch atomic |
| catch-up event batch | replay range | resume next sequence | source still active |
| validation receipt | rerun validation | reuse receipt | source still active |
| cutover CAS | source remains active | target remains active | old or new, never mixed |
| post-cutover bookkeeping | no cutover yet | recover forward | never roll pointer back |
| retirement CAS | bundle accepts pins | new pins rejected | old pins still readable |

DST explores concurrent submit/activate/retire, expired leases racing finish,
batch replay racing catch-up, source writes racing cutover, tombstones, empty and
full pages, budget exhaustion on every item, supervisor generation replacement,
and DST boundaries including spring-forward/fall-back timestamps. Eligibility
uses simulated logical time; production wall time may only schedule wakeups.

## Sanitized deterministic vector

`crates/temper-spec/tests/scoped_spec_bundle.rs` defines the public-safe v1
foundation vector using fictional `Example.Alpha`, `Example.Beta`, and
`task-42`. Ordered and reordered/whitespace-varied inputs both produce:

```text
sha256:3586b27317d263dc5d705c190da24b03a2a91b9a1d7a212ba1f3300273f4e512
```

The vector is intentionally free of answers, credentials, customer names, and
private runtime data. Any deliberate canonicalization change must introduce a
new contract version and new vector; silently updating this digest is forbidden.

## Task 114 completion checklist

- Preserve the v1 Cedar, WASM, migration, and budget digest frames when exposing
  submit.
- Add one governed service with HTTP/WASM parity and invocation-bound identity.
- Add one semantic store trait and make Postgres, Turso, and Sim pass the shared
  scenarios above.
- Consume task-58 pins and hot-reload contracts; do not recreate them.
- Implement pure sandbox verification, shadow migration, catch-up, validation,
  atomic cutover, and forward-only recovery.
- Run existing tenant-global compatibility tests plus the crash/DST matrix.
- Do not claim live completion without fork deployment and Datadog evidence.
