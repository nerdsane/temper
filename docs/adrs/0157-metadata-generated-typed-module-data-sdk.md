# ADR-0157: Metadata-Generated Typed Module Data SDK

- Status: Accepted
- Date: 2026-07-24
- Deciders: Temper core maintainers
- Supersedes: ADR-0099
- Related:
  - ADR-0057: Native Immutable File Read Plane
  - ADR-0134: Query Plane Read Contract
  - ADR-0142: Dispatch Acknowledges After Projection
  - ADR-0154: OData Read-Surface Truthfulness
  - `crates/temper-codegen/`
  - `crates/temper-wasm/src/host_trait.rs`
  - `crates/temper-wasm/src/engine/host_functions.rs`
  - `crates/temper-server/src/odata/`
  - `crates/temper-server/src/state/dispatch/wasm/local_tdata_host.rs`

## Context

ADR-0099 removed the TCP hop from same-process WASM calls by recognizing local
`/tdata` URLs and invoking the existing OData handlers in-process. That
preserved the governed write path and improved transport cost, but retained
HTTP and OData as the module programming model.

Modules still:

- construct entity-set, key, bound-action, query, batch, and File `$value` URLs;
- serialize request objects and deserialize loosely typed JSON;
- normalize CSDL names, entity-set names, action names, and property casing;
- classify HTTP status codes and parse OData error envelopes;
- copy tenant, bearer, principal, and observability headers through call layers;
- issue read-after-write calls without stating which committed sequence they
  must observe; and
- repeat this plumbing in every application module.

The local host wrapper makes those calls faster, but it cannot make them typed
or remove the duplicated protocol policy. It also leaves two internal entry
paths: OData route handlers own response-oriented behavior while local WASM
calls impersonate HTTP requests to reach it.

Temper already has the inputs needed to generate a stronger contract. Published
applications contain CSDL entity metadata, IOA action and parameter metadata,
and an explicit dependency closure. The server already carries the invoking
tenant and security context, dispatches through generic verification and Cedar
gates, persists monotonic per-entity sequence numbers, projects those sequence
numbers into the query plane, and provides native file paths.

The kernel needs one typed module data contract derived from that metadata. It
must not hardcode TemperPaw entities or weaken the public OData API.

## Decision

Generate an application-specific module SDK from canonical published metadata.
The generated SDK calls a versioned, transport-neutral WASM host ABI. The host
ABI and external OData handlers adapt into one shared governed application data
service.

OData remains the external interoperability surface. It is not the internal
module transport or the source language of generated module calls.

### Sub-Decision 1: Resolve And Lock Metadata Before Module Compilation

SDK generation happens before WASM compilation, not during publication. The
build pipeline uses this fixed order:

1. Resolve the root application and every named dependency to an immutable
   bundle version and content digest.
2. Write a deterministically ordered dependency lock containing application
   identity, version, bundle digest, and dependency edges.
3. Verify and merge the locked closure's CSDL entity/container metadata, IOA
   actions and parameters, declared File capabilities, and declared
   composite/atomic actions.
4. Produce the canonical closure schema plus one module-specific
   `ModuleSdkManifest`.
5. Generate that module's SDK, compile the WASM module, and package the binary
   with its manifest binding.
6. During publication and activation, resolve the lock again, recompute the
   canonical inputs and binding, and reject any mismatch before loading WASM.

Named dependencies that cannot be resolved to immutable versions and digests
are a build error. Publication does not silently re-resolve a name to a newer
bundle.

Ambient entity types discovered from a running tenant are not generation
inputs. A module can use another application's types only when the exact bundle
is in its locked dependency closure.

The per-module manifest is deterministically ordered and contains:

- SDK ABI version;
- locked-closure digest and module-schema digest;
- fully qualified entity type and entity-set names used by that module;
- property names, scalar types, nullability, enum members, and references;
- action names, binding types, parameter types, and result types;
- the module's exact operation capability grants;
- supported query and File capabilities; and
- explicit request, response, page, batch, item, and byte budgets.

Generated source includes typed entity identifiers, entity values, create and
patch inputs, action inputs and results, the closed v1 query expressions, page
cursors, commit tokens, File handles, and structured errors.

**Why this approach**: the SDK must exist to compile the module. Resolving
immutable inputs first removes that build-order cycle and makes generation
reproducible from the exact closure Temper later verifies and deploys.

### Sub-Decision 2: Availability And Authority Are Separate

The locked closure defines which schema symbols exist. Each module declaration
separately defines the subset it may call. The app build pipeline compiles those
declarations into a canonical grant list containing:

- allowed operation kinds;
- allowed entity types;
- allowed bound actions per entity type;
- queryable and orderable properties;
- allowed File operations;
- whether a declared composite action may be invoked; and
- per-operation page, item, payload, response, and stream budgets.

`app.toml [[wasm_modules]]` is the sole v1 owner of the grant. Extend the
existing module declaration with this shape:

```toml
[[wasm_modules]]
name = "task_worker"
target = "wasm32-wasip1"

[wasm_modules.data]
operations = [
  "entity_get",
  "entity_query",
  "entity_create",
  "entity_patch",
  "action_invoke",
  "batch",
  "file_read",
]

[[wasm_modules.data.entities]]
type = "Temper.ProjectManagement.Task"
actions = ["StartWork", "Complete"]
composite_actions = []
query_filter_fields = ["Status", "AssigneeId"]
query_order_fields = ["CreatedAt", "Id"]
query_order_by_sequence = true
file_operations = []

[[wasm_modules.data.entities]]
type = "Temper.FileSystem.File"
actions = []
composite_actions = []
query_filter_fields = []
query_order_fields = []
file_operations = ["metadata_read", "version_read", "content_read"]

[wasm_modules.data.budgets]
max_calls = 32
max_batch_items = 64
max_page_items = 100
max_request_bytes = 1048576
max_response_bytes = 4194304
max_open_responses = 4
max_open_streams = 4
max_stream_bytes = 67108864
```

Operation, action, property, and File-operation strings are closed v1 enums or
canonical schema names as appropriate. An entity entry grants no CRUD operation
unless `operations` contains it, and an operation grants no entity unless an
entity entry names it. Bound and composite actions require both their operation
kind and their exact per-entity action entry. Query fields and File operations
are per entity. All declared maxima must be positive and within platform
budgets.

There must be exactly one `[[wasm_modules]]` declaration per module name.
Duplicate declarations are invalid rather than merged. IOA `[[integration]]`
and `[[action.triggers]]` declarations select a module and supply trigger
configuration; they never add, union, or widen data grants. A module selected
by multiple triggers uses the one grant from its `app.toml` declaration for
every invocation.

The app-manifest parser validates syntax first. Closure verification then
resolves every entity, property, action, composite action, and File capability
against verified CSDL/IOA metadata before SDK generation. A grant cannot name a
symbol outside the locked closure. The generator emits only clients and methods
in the module's grant, while the host independently enforces the same canonical
grant at runtime.

The canonical grant digest is included in the module manifest binding and the
compiled artifact's content hash. It is not supplied in a data-call request.
Missing grants deny by default. An app-wide schema or dependency declaration
does not grant every module access to every symbol.

**Why this approach**: generated availability is not authorization. A
module-specific least-privilege surface both reduces accidental coupling and
gives the host an exact fail-closed capability check.

### Sub-Decision 3: Generated Names Own Casing

The manifest records both canonical wire names and generated language names.
Code generation applies the naming conversion once and emits explicit mappings.
Runtime code does not guess casing and modules do not normalize names.

For Rust guests, generated fields and methods use idiomatic `snake_case`.
Generated serializers map them to the exact CSDL/IOA names. Entity-set names
and fully qualified action names are constants hidden behind typed clients.
Name collisions after normalization are a generation error that identifies all
conflicting canonical names.

**Why this approach**: casing is schema information. Repeating heuristic
normalization in modules makes correctness depend on which caller constructed a
request.

### Sub-Decision 4: Use One Concrete Versioned Data Host ABI

Add a host capability for application data operations, separate from
`host_http_call`. The guest-facing generated SDK exposes typed methods; its
runtime serializes the v1 UTF-8 JSON request envelope and decodes the v1 UTF-8
JSON response envelope. All v1 structs use `deny_unknown_fields`; enum tags and
field names are lowercase `snake_case`. Canonical entity/property/action names
remain case-sensitive string values selected by generated code.

The request has exactly this top-level shape:

```text
DataRequestV1 {
    abi: 1,
    operation: DataOperationV1
}

DataOperationV1 =
    EntityGet { entity_type, entity_id, at_least_sequence? }
  | EntityQuery { entity_type, filter?, order_by[], page }
  | EntityCreate { entity_type, value }
  | EntityPatch { entity_type, entity_id, expected_sequence?, value }
  | ActionInvoke { entity_type, entity_id, action, expected_sequence?, params }
  | Batch { items[] }
  | CompositeInvoke { entity_type, entity_id, action, expected_sequence?, params }
  | FileReadOpen { file_id, version_id? }
  | FileWriteOpen { file_id, expected_sequence?, content_length?, content_hash? }
  | FileWriteCommit { stream_handle }
  | FileStreamAbort { stream_handle }
```

Every sum type is adjacently tagged by a `kind` field with its fields in the
same object. For example:

```json
{"abi":1,"operation":{"kind":"entity_get","entity_type":"Temper.App.Task","entity_id":"task-1","at_least_sequence":7}}
```

`value` and `params` are JSON objects whose exact property types are validated
against the host-bound module manifest before service dispatch. A batch item is
one of `EntityGet`, `EntityCreate`, `EntityPatch`, or `ActionInvoke`; nested
batches, streams, and composite actions are rejected.

The v1 query contract is deliberately smaller than OData:

```text
FilterV1 =
    Compare { field, operator: eq | ne | lt | le | gt | ge, value: ScalarV1 }
  | IsNull { field, is_null: bool }
  | And { operands[] }
  | Or { operands[] }
  | Not { operand }

ScalarV1 =
    Boolean(bool)
  | Int64(i64)
  | Double(f64)
  | String(string)
  | Guid(canonical-string)
  | DateTimeOffset(RFC-3339-string)
  | Decimal(canonical-string)
  | Enum { type_name, member }

OrderV1 = Property { field, direction: asc | desc }
        | EntityCommitSequence { direction: asc | desc }
PageV1 { limit, cursor? }
```

Comparisons are allowed only when the generated property type supports the
operator. Null has no scalar encoding and is tested only through `IsNull`.
`And` and `Or` require at least one operand and are depth/count bounded.
Ordering uses the existing query-plane scalar/null semantics and adds entity ID
ascending as the stable tiebreaker. The cursor is opaque host output and must be
replayed with the same filter and ordering. V1 has no raw filter strings,
navigation, expansion, arbitrary functions, offset, or caller-constructed
cursor.

The response has exactly this top-level shape:

```text
DataResponseV1 {
    abi: 1,
    outcome: Ok(DataResultV1) | Err(ModuleDataError)
}

DataResultV1 =
    Entity { value, sequence }
  | Page { values: [{ value, sequence }], next_cursor? }
  | Write { commit, value?, value_omitted: bool }
  | Action { commit, result?, result_omitted: bool }
  | Batch { outcomes[] }
  | FileRead { stream_handle, metadata, sequence, content_length? }
  | FileWrite { stream_handle }
  | FileCommitted { commit, metadata? }
  | Aborted
```

The two outcome encodings are exactly:

```json
{"abi":1,"outcome":{"kind":"ok","result":{"kind":"entity","value":{},"sequence":7}}}
{"abi":1,"outcome":{"kind":"error","error":{"kind":"not_found","code":"EntityNotFound","message":"entity not found","retryability":"never"}}}
```

The Preview 1 imports are:

```text
host_temper_data_call(
    request_ptr: i32,
    request_len: i32
) -> i64

host_temper_data_response_len(response_handle: i32) -> i32

host_temper_data_response_read(
    response_handle: i32,
    offset: i32,
    buffer_ptr: i32,
    buffer_capacity: i32
) -> i32

host_temper_data_response_close(response_handle: i32) -> i32

host_temper_file_stream_read(
    stream_handle: i32,
    buffer_ptr: i32,
    buffer_capacity: i32
) -> i32

host_temper_file_stream_try_write(
    stream_handle: i32,
    data_ptr: i32,
    data_len: i32
) -> i32
```

`host_temper_data_call` copies a request from guest memory, completes the
bounded async operation, stores the encoded response in a host-owned,
invocation-scoped response registry, and returns a positive handle. It returns:

- `-1` for zero/negative request length or a pointer/length range outside guest
  memory;
- `-2` when the raw request exceeds the module's request-byte budget;
- `-3` when the bounded response-handle registry has no capacity; and
- `-4` for a host trap or exhausted invocation deadline before a response can
  be registered.

Domain, authorization, schema, consistency, and operation-budget failures are
encoded as `DataResponseV1::Err`, not negative ABI codes.

All pointer arguments are interpreted as WebAssembly linear-memory offsets.
Address zero is valid. Every pointer/length pair is overflow checked and must
fit the current memory before a read or write.

The request region remains guest-owned and the host does not retain pointers
after the initial bounded copy. Positive response handles fit in `i32`; the
`i64` return leaves negative ABI codes unambiguous. Encoded response bytes
remain host-owned. The buffer passed to `response_read` remains guest-owned and
the host writes at most `buffer_capacity` bytes.

The guest calls `response_len`, which returns the non-negative encoded length or
`-1` for an invalid/closed handle, allocates exactly that bounded number of
bytes, then uses `response_read`. `response_read` requires non-negative offset
and capacity; it returns bytes copied, `0` for zero capacity or at end, and `-1`
for invalid memory/range/handle. Reading never re-executes the operation.
`response_close` returns `0` and releases the handle, or `-1` for an
invalid/already-closed handle. All response handles are released when the
invocation ends.

The host rejects `request_len` above the bound before copying or parsing guest
memory. That raw byte bound limits allocations made by `serde_json`. After
parsing, the host validates item counts, string lengths, object depth, and
payload bytes against the manifest before dispatch. It does not claim that
ordinary Serde enforces element budgets before allocation.

The response maximum plus a fixed compact acknowledgement/error allowance is
reserved from the invocation budget before dispatch. Encoding uses a bounded
writer, charges actual bytes, and releases unused reservation. If a read result
exceeds its reservation, the host returns `BudgetExceeded`. If a write has
already committed, it returns `Write` or `Action` with the commit token,
`value_omitted` or `result_omitted` set, and no oversized value; it never reports
the committed write as failed.

File stream handles use the existing bounded `StreamRegistry` substrate but
have data-specific imports and typed registry entries:

```text
FileStreamEntry {
    owning_invocation,
    module_artifact_digest,
    direction: file_read | file_write,
    state: open | consumed,
    remaining_byte_budget
}
```

The host chooses a bounded slot/generation handle; the guest cannot choose the
entry metadata. Host state is invocation-scoped, and every read, write, commit,
or abort verifies the current invocation, loaded module artifact, File
capability, direction, open state, and remaining budget before touching bytes.
Response handles and inbound/outbound HTTP stream handles are ineligible even
when their integer value matches a File handle. A read call rejects write,
foreign, HTTP, response, or consumed handles; write, commit, and abort apply the
corresponding direction checks.

`file_stream_read` returns bytes read, `0` for EOF, `-1` for `WouldBlock`, `-2`
for closed, `-3` for invalid/wrong-owner/wrong-kind/wrong-direction/consumed
handle, and `-4` for abort or memory error. EOF atomically consumes the read
entry. `file_stream_try_write` uses the same negative codes and returns bytes
accepted.

A write is durable only after `FileWriteCommit`, which atomically changes one
open write entry to consumed before dispatching the commit and cannot be called
twice. `FileStreamAbort` atomically consumes an open read or write entry and
discards uncommitted bytes. Invocation termination consumes every remaining
entry. There is no implicit File close import: commit and abort are explicit
data operations so closing or confusing a handle cannot accidentally commit
bytes.

The request carries no tenant, principal, capability grant, manifest digest,
schema compatibility proof, URL, method, header, status, or OData error field.
Those values come from the host-bound artifact and authority snapshot.

External provider and webhook calls continue to use `host_http_call`. The new
ABI is only for governed Temper application data.

**Why this approach**: a small versioned ABI fits the current Preview 1 WASM
runtime and permits future language SDKs. It also allows a later move to WIT
components without making URLs and OData shapes part of the durable guest
contract.

### Sub-Decision 5: OData And The SDK Share A Governed Service

Extract a transport-neutral application data service inside `temper-server`.
It accepts resolved tenant, security context, entity/action identity, typed
parameters, consistency requirements, and explicit budgets. It returns domain
results with commit sequences or structured errors.

Both adapters use that service:

- OData parses paths, query options, headers, and JSON; calls the service; then
  renders OData status, headers, annotations, next links, and error envelopes.
- The module host validates its ABI request; calls the same service; then
  renders the SDK response envelope.

The shared service owns:

- entity and action resolution;
- Cedar collection, row, and action authorization;
- IOA transition, guard, invariant, relation, and deterministic-ID checks;
- actor dispatch and composite transaction selection;
- persistence and projection acknowledgement;
- query-plane planning and authoritative fallback;
- audit, trajectory, and WideEvent emission; and
- operation budgets.

The service does not accept URLs or return HTTP responses. OData-only features
such as `$metadata`, service documents, annotations, and response status mapping
remain in the OData adapter.

**Why this approach**: calling handlers directly preserves behavior only while
HTTP remains the internal abstraction. A shared governed service preserves
semantics without forcing either adapter to impersonate the other.

### Sub-Decision 6: Authorization Comes From A Host-Only Snapshot

Introduce a server-owned `ModuleInvocationAuthority` snapshot containing:

- resolved `TenantId`;
- resolved Cedar `SecurityContext`;
- loaded module artifact identity;
- module capability-grant digest;
- triggering entity type and action; and
- invocation operation/byte/stream budgets.

The snapshot is constructed before WASM entry and stored only in host state. It
does not implement `Serialize`, is never placed in guest memory, and is distinct
from the existing guest-visible `WasmInvocationContext`. Fields such as
`tenant`, `agent_id`, and HTTP principal information visible to the guest remain
inputs/observability data and are not authority.

For an externally initiated operation, the snapshot copies the already resolved
`AgentContext.security_ctx`. If that context is absent, module data host
construction fails closed with `AuthorizationDenied`; it does not synthesize an
anonymous or system principal.

An internal workflow without an originating principal must explicitly construct
a named service identity through the existing `AgentContext::for_service`
boundary before module invocation. The service name is audited and evaluated by
Cedar. The module host never silently upgrades a missing principal to
`SecurityContext::system()`.

Module data requests cannot supply tenant, principal, bearer token, agent role,
security headers, capability digest, or artifact identity. The host reads those
values from `ModuleInvocationAuthority` when constructing the service request.

Authorization occurs at two layers:

1. A module capability gate evaluates the module identity, trigger action,
   requested operation, entity type, and bound action against the deployed
   artifact's exact module grant bound into the authority snapshot.
2. The shared service applies the same Cedar collection, row, and action checks
   that protect the corresponding OData operation, using the invoking
   principal.

Generated clients do not accept auth parameters. Internal service calls never
mint or parse a bearer token. HTTP adapters continue to authenticate normally
before constructing their security context.

**Why this approach**: headers are evidence at a network trust boundary, not an
appropriate internal identity mechanism. Separating guest-visible invocation
data from the host-only authority snapshot prevents a guest from changing
tenant, principal, artifact, or capability while preserving Cedar enforcement.

### Sub-Decision 7: Errors Are Structured Before HTTP Mapping

The shared service and SDK use a stable `ModuleDataError` shape:

```text
kind          closed, low-cardinality category
code          stable machine-readable code
message       bounded safe explanation
retryability  never | after_refresh | with_backoff
decision_id   optional governance decision identifier
details       optional typed, size-bounded metadata
```

The initial error kinds are:

- `InvalidRequest`;
- `SchemaMismatch`;
- `NotFound`;
- `AlreadyExists`;
- `AuthorizationDenied`;
- `GuardRejected`;
- `RelationViolation`;
- `VerificationFailed`;
- `Conflict`;
- `ConsistencyUnavailable`;
- `BudgetExceeded`;
- `TransientUnavailable`; and
- `Internal`.

The OData adapter maps these categories to HTTP status codes and OData error
envelopes. The SDK adapter returns them directly. Raw persistence errors,
internal URLs, bearer values, policies, stack traces, and unbounded entity state
must not cross either boundary.

Batch results carry `Result<T, ModuleDataError>` per input item. A transport or
envelope failure is returned once for the whole request.

**Why this approach**: HTTP status classes lose distinctions modules need and
make retry behavior a caller convention. A domain error can be rendered as HTTP
without making HTTP the source of truth.

### Sub-Decision 8: Writes Return Commit Tokens

Every successful create, patch, action, composite action, or file metadata write
returns the resulting entity and:

```text
CommitToken {
    entity_type,
    entity_id,
    sequence
}
```

Tokens are scoped to one tenant and one entity stream. The tenant is bound by
the host context and is not serialized into the token. A token is a consistency
claim, not an authorization credential: changing it cannot grant access or
select another tenant. The host rejects a token whose entity type or identifier
does not match the requested entity. A caller-supplied future sequence can only
consume the already charged bounded read and produce
`ConsistencyUnavailable`.

The generated client records the highest token it observes for each entity
during an invocation. A subsequent keyed read of that entity automatically
requests at least that sequence. Callers may also pass an explicit token across
module steps when it is part of durable workflow data.

**Why this approach**: a successful write already has an authoritative sequence.
Discarding it forces callers to infer consistency from timing.

### Sub-Decision 9: Keyed Reads Can Require A Sequence

Keyed reads and bounded keyed-read batches accept an optional
`at_least_sequence` requirement. The service follows one bounded decision:

1. Serve the projected row when its sequence is at least the requirement.
2. Otherwise load the authoritative actor/event state once through the existing
   bounded path.
3. Return `ConsistencyUnavailable` if authoritative state cannot satisfy the
   token within the operation budget.

The server does not sleep, spin, or retry until a projection catches up. The
guest SDK does not poll. An impossible token for the resolved entity is rejected
as `ConsistencyUnavailable`; a token for a different entity is
`InvalidRequest`.

Collection queries return a sequence with every row and retain OData's
continuation semantics, but they do not claim a tenant-wide snapshot or accept
one entity's commit token as a collection watermark.

This strengthens the residual projection-failure case documented by ADR-0142
without changing its synchronous projection acknowledgement.

**Why this approach**: consistency is a per-stream fact. One authoritative
fallback is deterministic and bounded; timing-based polling is neither.

### Sub-Decision 10: Batches Are Bounded And Explicitly Non-Atomic

The generated SDK exposes bounded homogeneous keyed-read batches and a bounded
general operation batch. Inputs are assigned stable positions and results
preserve request order.

An ordinary batch is non-atomic. Each item has an independent result, and a
failure does not imply rollback of successful siblings. The host consumes the
declared operation, entity, and payload budgets and reserves the declared
maximum response bytes before dispatch. Results consume that reservation as
they are encoded; unused bytes are released. A batch that cannot reserve its
full declared maximum is rejected before any item runs.

Atomic behavior is available only through a generated method for an action
whose verified metadata declares the existing composite atomic contract. The
SDK must not label a general batch as a transaction.

Simulation executes the same deterministic request order. Production may use a
bounded storage primitive or bounded concurrency only where it cannot change
observable ordering, authorization, conflict, or audit semantics.

**Why this approach**: batching removes repeated crossings of the WASM boundary.
It must not silently invent transactional semantics or unbounded fan-out.

### Sub-Decision 11: File Content Uses Native Stream Handles

Generated File clients separate typed metadata operations from content streams.
Metadata reads, actions, versions, and commit tokens use the application data
service. Content opens a bounded host-managed byte stream and reuses the native
blob/file authorization and persistence path.

Modules do not construct `$value` URLs, encode bytes in JSON, or receive the
blob endpoint and API key. Stream handles are invocation-scoped, capacity
bounded, and closed on invocation termination.

Successful content writes return the resulting File commit token only after the
same metadata/action acknowledgement required by the public File path.

**Why this approach**: File bytes should not be buffered into a generic entity
envelope. Existing native streaming is the appropriate substrate; the generated
client supplies the typed lifecycle.

### Sub-Decision 12: Bind Compatibility To The Loaded Artifact

Each compiled module artifact carries a host-readable `ModuleSdkBinding`:

```text
ModuleSdkBinding {
    abi_version,
    locked_closure_digest,
    module_schema_digest,
    capability_grant_digest,
    used_symbols_digest,
    generator_version
}
```

The binding and canonical used-symbol list are artifact metadata covered by the
module artifact's content hash. The guest request contains none of these fields.
The server selects the binding from the exact artifact it loaded; guest memory
cannot select or replace it.

During publication and hot reload, the server resolves the artifact's pinned
dependency lock, recomputes the closure schema and module capability grant, and
checks every binding digest before activation.

Activation fails deterministically when:

- the host does not support the ABI version;
- the pinned dependency lock cannot be reproduced;
- a referenced entity, property, action, or enum member is missing or
  incompatible;
- a required file, query, batch, or composite capability is unavailable; or
- any binding digest differs without a valid compatibility proof.

A build may package a compatibility proof for a compatible additive schema
change. The proof contains the prior and candidate closure digests plus the
canonical type/semantic hashes of every used symbol. It is covered by the
artifact content hash. Activation recomputes those hashes from the pinned
closure and accepts the proof only when every used symbol is unchanged and the
capability grant is equal or narrower. A proof received from the guest envelope
is an unknown field and is rejected. Version-number comparison or a
generator-supplied assertion without host recomputation is insufficient.

Existing running instances are not hot-swapped to an incompatible artifact.
The rejection emits a bounded diagnostic and leaves the prior deployment
active.

**Why this approach**: generated types move schema errors to build/deploy time
only if the runtime verifies which schema they represent.

### Sub-Decision 13: Preserve Audit And Add Adapter-Neutral Telemetry

SDK calls emit the same transition, authorization, trajectory, persistence, and
projection evidence as equivalent OData calls. The shared service records one
operation span; adapters add their own boundary span.

Module data spans use low-cardinality fields for ABI version, operation kind,
entity type, action, result kind, consistency path, and bounded batch counts.
They do not record entity identifiers, file contents, auth material, or full
payloads by default.

The old local-TData span remains during migration so production traces can
compare call volume and latency. It is removed with the wrapper after no
packaged module requires it.

**Why this approach**: removing HTTP should remove transport overhead, not the
evidence used to govern and diagnose application behavior.

### Sub-Decision 14: Make Offline Candidate Builds App-Rooted And Explicit

Provide two offline `temper module-sdk` commands. The natural invocation names
an app root and module, then lets Temper derive repository conventions from that
root:

```text
temper module-sdk generate --app /repo/os-apps/paw-heal --module heal_reporter \
  --dependency-root /repo/os-apps

temper module-sdk bind --app /repo/os-apps/paw-heal --module heal_reporter \
  --dependency-root /repo/os-apps \
  --wasm /repo/target/wasm32-wasip1/release/heal_reporter.wasm
```

`--app` is an explicit filesystem anchor, never an app name resolved through a
server or catalog. A relative `--app` is resolved once against the caller's
working directory and canonicalized. Relative override paths are then resolved
exactly once against that canonical app root; after that, all
discovery is constrained beneath the canonical app and dependency roots. The
commands never search upward from the current directory and never consult
Temper server state, Genesis, environment-selected app catalogs, or HTTP.

By convention, generation writes
`wasm/<module>/src/temper_module_sdk.rs` and `temper-module-sdk.lock` beneath the
app root. Binding writes `wasm/<module>/<module>.wasm` and updates the exact
`app.toml` beneath that root. Explicit `--source-out`, `--lock`,
`--bound-wasm-out`, and `--app-manifest` overrides support nonstandard layouts;
every override must still be named rather than inferred from the current
directory. The commands print the resolved inputs and outputs so CI and humans
can see the complete build contract.

A dependency root is an explicitly supplied directory containing one or more
app directories. Resolution reads only the root app's declared dependency
graph and matches each dependency to exactly one supplied local app manifest.
Missing dependencies, duplicate candidates, cycles, manifest-name mismatches,
unsafe paths, and conflicting CSDL/IOA symbols fail closed. Resolution parses
and validates every declared app manifest and IOA automaton and the single
unambiguous CSDL document in each app, merges the
closure in deterministic dependency order, and rejects incompatible duplicate
entity, enum, action, function, term, entity-set, action-import, or
function-import symbols rather than relying on merge precedence. Unrelated apps
under a dependency root are indexed by directory name but never parsed.

The v1 resolver applies explicit budgets before allocation: at most 32
dependency roots, 4,096 directory entries per dependency root, 1,024 candidate
apps, 128 apps in the declared closure, 1,024 metadata directory entries, and
256 IOA files per app. Individual manifests are capped at 1 MiB, CSDL at 8 MiB,
IOA files at 2 MiB, and aggregate metadata at 32 MiB per app. Exceeding a budget
fails before an unbounded read or closure merge.

Compiled, existing bound, and final bound WASM artifacts are individually
capped at 256 MiB. Output paths are normalized before use and must be distinct
from the app manifest, generated source, lock, and compiler input; aliases fail
before any read or write.

The generated TOML lock contains the resolver version, root identity, stable
dependency edges, app versions, and canonical metadata digests. Candidate
metadata digests exclude generated SDK source, final data bindings, and compiled
WASM so the pre-compilation lock has no digest cycle. They include every
generation-relevant app manifest grant, CSDL document, IOA specification, and
declared dependency edge. Canonical CSDL emission sorts record annotations
before schema hashing; canonical ordering and length-framed hashing make
identical inputs byte-for-byte reproducible.

`generate --check` recomputes the closure and fails without writing if either
the lock or generated source differs. `bind` recomputes the same closure and
source expectation, packages the supplied unbound WASM with the exact binding,
writes the conventional final artifact, and updates the module's manifest
binding without discarding unrelated formatting or comments. `bind --check`
performs all regeneration and binding checks in memory and fails without
writing on source, lock, grant, binding, manifest, input-WASM, or final-WASM
drift. Binding an already-bound input fails closed. Final artifact and manifest
outputs are staged before publication; if manifest publication fails after the
artifact rename, the prior artifact is restored so the app never points at a
partially published binding.

Local locks are candidate evidence, not publication authority. They let a
consumer compile before it can contact Genesis. When Genesis later publishes
or resolves the app, its immutable published bundle and dependency closure are
authoritative. If they diverge from the local candidate, publication and
activation reject the candidate binding; tooling must regenerate and rebuild
against the Genesis closure rather than silently preserving local metadata.

Entity commit sequence is a typed query-order target, not a reserved property
name. A module receives the generated `commit_sequence` order constructor only
when its entity grant declares `query_order_by_sequence = true`. The host carries
the typed target through query validation and storage planning; Turso and
PostgreSQL order by their host-owned `entity_catalog.sequence_nr` column, and
the bounded authoritative fallback compares hydrated `sequence_nr` values.
Callers cannot supply a fake entity property to impersonate the sequence.
Generation rejects a granted CSDL property whose Rust order constructor would
also normalize to `commit_sequence`.
Property ordering retains its existing ABI representation and semantics, and
external OData continues to expose only declared CSDL properties.

**Why this approach**: one app-root anchor makes the common workflow concise,
while explicit dependency roots and typed order targets keep resolution and
host-owned metadata fail closed. Separate unbound input and conventional bound
output make drift checks repeatable and prevent double-binding corruption.

## Rollout Plan

1. **ADR PR** — Merge this decision and mark ADR-0099 Superseded. No runtime
   behavior changes in the ADR PR.
2. **Temper kernel PR** — Add the manifest and generator, versioned host ABI,
   shared governed service, structured errors, sequence-aware reads, bounded
   batches, File streams, compatibility checks, and tests. Retain the
   ADR-0099 wrapper as a migration fallback.
3. **TemperPaw PR** — Resolve and lock its dependency closure, generate each
   module's SDK and binding before compilation,
   migrate application modules, and remove module-owned local URL, JSON, status,
   casing, polling, and auth plumbing.
4. **Removal PR** — After packaged-module inventory proves no internal module
   calls local `/tdata`, remove `LocalTDataWasmHost` and stop injecting
   `temper_api_url` and `temper_api_key` into modules. Keep `host_http_call` for
   external services and keep all public OData handlers.
5. **Production proof** — Deploy each repository in dependency order, verify
   versions, run direct and routed workflows plus File and batch probes, and
   compare Datadog correctness and latency signals.

## Readiness Gates

- Generated code compiles for representative scalar, enum, nullable,
  reference, action, batch, and File metadata.
- Generation is byte-for-byte deterministic for equivalent canonical metadata.
- Dependency names resolve to an immutable lock before generation, and
  publication reproduces the same lock instead of resolving newer bundles.
- Normalized-name collisions and unsupported schema constructs fail generation.
- Preview 1 ABI conformance tests cover exact v1 JSON, unknown fields/versions,
  raw request bounds, guest-memory failures, response handle capacity,
  read/close lifecycle, compact post-commit acknowledgement, and invocation
  cleanup.
- The direct ABI cannot select another tenant, principal, artifact, schema
  digest, compatibility proof, or capability grant.
- A module receives generated methods only for its canonical grant, and runtime
  tests prove an ungranted entity/action/operation fails closed.
- Manifest tests prove trigger declarations cannot widen a module grant and
  duplicate module declarations fail instead of merging.
- Missing originating authority fails closed; named internal service principals
  are explicit and audited.
- Capability and Cedar denials are covered independently.
- Query tests cover every v1 scalar and operator, null semantics, depth/count
  bounds, stable ordering, opaque cursor replay, and rejection of raw OData.
- OData and SDK adapters have parity tests for create, keyed read, patch,
  action, guard failure, authorization denial, conflict, and not-found behavior.
- Writes return the committed sequence and a subsequent SDK keyed read observes
  at least that sequence without polling.
- Projection-lag simulation covers projected success, authoritative fallback,
  impossible tokens, and bounded failure.
- Batch tests prove bounds, request-order results, partial failure, and the
  distinction from declared atomic composite actions.
- File tests cover bounded streaming, authorization, versions, cleanup, and
  committed-sequence acknowledgement. They also prove response, HTTP,
  wrong-direction, foreign-artifact, and consumed handles are rejected and
  commit/abort/EOF consume exactly once.
- Activation recomputes closure, schema, grant, and used-symbol hashes; stale or
  guest-supplied bindings/proofs fail while the prior deployment stays active.
- Existing public OData metadata, CRUD, query, action, paging, and `$value`
  behavior remains green; the SDK batch contract does not imply an OData
  `$batch` endpoint.
- `cargo test --workspace`, `cargo clippy --workspace`, and `cargo fmt --check`
  pass.
- Mandatory DST and code-quality reviews pass.
- Live direct/routed module workflows preserve entity chains and File contents.
- Datadog shows no lost authorization, transition, projection, or audit evidence
  and distinguishes SDK calls from external OData and HTTP calls.

## Consequences

### Positive

- Module code uses generated domain types instead of transport strings.
- Entity/action naming and casing failures move to generation time.
- Internal calls no longer construct URLs, headers, HTTP status policies, or
  OData envelopes.
- Read-your-writes requirements become explicit and sequence based.
- Batches cross the WASM boundary once while retaining bounded partial-failure
  semantics.
- OData remains stable for external clients.
- The shared service reduces semantic drift between internal and external
  callers.

### Negative

- The kernel gains an ABI and generated SDK compatibility surface.
- OData handlers must be separated from domain execution without changing their
  externally observable behavior.
- Published artifacts become larger and deployment validation becomes stricter.
- During migration, the direct SDK and ADR-0099 local wrapper coexist.
- Cross-language SDKs require generators over the same manifest.

### Risks

- **Shared-service extraction changes OData behavior.** Mitigate with
  characterization and adapter parity tests before removing route-local logic.
- **Generated types omit a schema feature.** Fail generation explicitly; never
  fall back to untyped maps for an unsupported required field or action.
- **SDK authorization becomes broader than HTTP authorization.** Keep the
  module capability gate and shared Cedar checks separate and test both.
- **Sequence fallback creates unbounded actor work.** Permit one bounded
  authoritative load, never a retry loop.
- **Batch calls create hidden fan-out.** Charge all declared budgets before
  dispatch and preserve stable result order.
- **Schema digest blocks safe additive releases.** Allow only a generator-issued
  proof bound into the artifact and independently recomputed by the host over
  the module's exact used-symbol set.
- **Two internal paths drift during migration.** Compare parity and telemetry,
  then remove the ADR-0099 wrapper promptly after inventory reaches zero.

### DST Compliance

- `temper-server` and `temper-wasm` are simulation-visible.
- Manifest, capability, batch, and used-symbol collections use deterministic
  ordering.
- The simulator implements the same data ABI with seeded state and stable
  request order.
- Sequence-aware reads do not use sleeps, wall-clock deadlines, or retry loops.
- Batches are capacity bounded and cannot introduce unbounded tasks.
- File handles and payload buffers have explicit byte and count budgets.
- The design adds no random identifiers, ambient filesystem/network access,
  multi-threaded actor scheduling, or process-global mutable state.

## Non-Goals

- Do not remove or narrow the public OData API.
- Do not expose TemperPaw-specific entities or workflow primitives from the
  kernel.
- Do not make arbitrary external HTTP APIs typed by this SDK.
- Do not treat an ordinary batch as an atomic transaction.
- Do not promise collection-wide snapshot isolation from per-entity sequences.
- Do not replace IOA/CSDL as the source of application behavior and data shape.
- Do not introduce runtime tenant schema discovery into generated modules.
- Do not migrate application modules in the Temper kernel repository.

## Alternatives Considered

1. **Keep ADR-0099 and add helper functions** — Rejected. Helpers reduce
   duplicated syntax but retain URLs, OData JSON, status mapping, auth headers,
   and ambiguous read consistency as the internal contract.
2. **Generate an OData HTTP client** — Rejected for internal modules. It would
   improve typing but preserve loopback transport concepts and repeat network
   identity plumbing. Such a client may still be useful to external consumers.
3. **Add entity-specific host functions** — Rejected. Kernel host imports must
   be metadata driven and cannot encode TemperPaw or other application names.
4. **Call actors directly from generated clients** — Rejected. That would
   bypass shared Cedar, query, relation, persistence, projection, and audit
   behavior.
5. **Adopt the WASM component model and WIT first** — Deferred. WIT is a viable
   future ABI representation, but requiring a runtime migration before removing
   current duplication expands the critical path. The manifest and closed
   operation model are designed to map to WIT later.
6. **Hide polling inside the SDK** — Rejected. Timing-based retries remain
   nondeterministic and consume budgets without proving which state was read.
7. **Use one global sequence watermark** — Rejected. Temper persistence and
   projections have per-entity streams; a global token would claim ordering the
   runtime does not provide.

## Rollback Policy

The direct data host is introduced alongside the ADR-0099 wrapper. Before
wrapper removal, rollback selects the prior host construction and regenerates
modules against the previous artifact without changing persisted entity data.

After wrapper removal, rollback redeploys the last compatible Temper and
TemperPaw artifact pair. The manifest, ABI envelopes, and commit tokens do not
change event or projection storage formats. Public OData and external
`host_http_call` remain available throughout.

If shared-service extraction causes OData parity failures, revert the adapters
to the prior handler-owned execution while retaining the ADR and failing the SDK
capability closed. Do not route SDK calls through loopback HTTP as a silent
fallback.
