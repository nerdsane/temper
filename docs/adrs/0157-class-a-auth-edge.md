# ADR-0157: Credential-Bound Class A Authentication Edge

- Status: Accepted
- Date: 2026-07-06
- Revised: 2026-07-11 after adversarial validation of PR #343
- Deciders: Temper core maintainers
- Related: ADR-0033, ADR-0043, ARN-165, ARN-166, ARN-167, ARN-170, ARN-187, ARN-192, ARN-219, ARN-231, ARN-233

## Context

Temper historically reconstructed Cedar authority from request headers. That
made `x-temper-principal-*`, `x-temper-agent-*`, `x-temper-attr-*`, and
`x-tenant-id` part of the security boundary even though an HTTP client or WASM
guest could supply them.

The first version of this ADR attempted to make those headers safe by stripping
them at one router edge, adding an internal "trusted" marker, and materializing a
credential-derived principal back into headers. Validation showed that this was
still a header trust model with several independent bypasses:

1. A WASM guest could overwrite the inherited principal and tenant on the
   in-process local-TData path. The host then added the trusted marker, turning
   the guest's values into Admin authority. A focused Cedar/OData test changed a
   denied Customer create into a successful cross-tenant Admin create.
2. The exported kernel `build_router` did not install the strip layer. A caller
   that supplied the marker could still reach Admin-only behavior. There is no
   current production direct embed, but the API made a future unsafe embed easy.
3. When `TEMPER_API_KEY` was absent, the network server passed requests through
   as `Customer::anonymous`. Shipped any-principal policies grant that principal
   real read and mutation capabilities, so "unprivileged" was not fail-closed.
4. Every `ProductionWasmHost` received the deployment-wide API key. A permitted
   guest could select another tenant and use that ambient credential as a
   cross-tenant Admin without knowing the key.
5. The internal blob HTTP handlers ignored the authenticated tenant and always
   addressed `default`, while the WASM Cedar gate treated any loopback URL
   containing `/_internal/blobs` as trusted. A non-default credential could
   therefore address default-tenant storage, and a near-match URL on another
   local service could inherit the blob exception.
6. Management authorization was split and, in several cases, absent. The admin
   router accepted only in-process Admin/System kinds even though network
   credentials intentionally resolve to Agent, while tenant, Genesis install,
   REPL, and server-local spec-directory routes allowed any authenticated
   credential to reach destructive or cross-tenant operations. A duplicate
   `/observe/tenants/{id}` deletion route also bypassed the path-tenant guard.
7. Inline spec submission joined caller filenames onto a predictable temporary
   directory and could write through `..` or absolute paths. Its optional
   `cedar_policies` field activated arbitrary policy text after only
   `submit_specs`, bypassing `manage_policies`, durable policy rows, and the
   approval transaction.
8. Tenant-binding the compatibility blob route was insufficient: any valid
   tenant credential could still read or write an arbitrary object key without
   a Cedar decision.
9. Native CLI adapters minted permanent `AgentCredential` rows with no expiry or
   revocation. A token captured from the child environment remained valid after
   the invocation, and a missing minted credential let the child inherit the
   server's deployment-wide `TEMPER_API_KEY`. This is ARN-231's permanent
   adapter-credential path and part of ARN-170's Class A boundary.
10. The isolated spec-verification subprocess inherited the server's complete
    environment, including deployment, database, and provider credentials, and
    its timeout dropped the wait future without configuring the child to die.

Loopback transport, header order, and possession of a deployment secret are not
identity. Authority must be derived once from a credential and remain typed and
tenant-bound until the operation is authorized.

## Decision

### 1. Authority is an immutable typed request context

Introduce `AuthenticatedRequestContext`, containing a `TenantId` and the exact
`SecurityContext` produced by credential resolution. Its fields are private and
it is carried as an axum request extension or a direct in-process argument.

Handlers consume this context; they do not reconstruct authority from headers.
`SecurityContext::from_headers` remains only for explicitly untrusted/anonymous
compatibility paths and cannot produce Customer, Agent, Admin, System, scopes,
roles, ABAC attributes, or action provenance from caller input.

The edge still removes the complete `x-temper-*` authority namespace as defense
in depth. Only the `x-temper-observe-*` and `x-temper-workflow-*` correlation
namespaces survive, and those values are never read as Cedar authority. There is
no trusted-principal header, materialization middleware, or
`PreAuthenticatedRequest` bypass.

### 2. Bearer authentication is tenant-scoped and fails closed

For every protected network request, middleware:

1. validates the requested tenant,
2. resolves the bearer token in that tenant's `AgentCredential` registry,
3. constructs one `AuthenticatedRequestContext`, and
4. rejects the request with 401 when any step fails.

The deployment `TEMPER_API_KEY` is bootstrap material, not ambient Admin
authority. Bootstrap may register it as the verified `operator` AgentType in an
explicit tenant. The normal tenant resolver then handles it exactly like any
other credential. Registering the same operator in another tenant is an explicit
administrative act; a match in one tenant never authorizes another.

There is no no-key network pass-through. The only unauthenticated routes are the
exact liveness/service-discovery endpoints and the credential-resolution
bootstrap endpoint. Local development that needs protected routes must create a
credential or opt into a separately named loopback-only development server; it
must not silently become `Customer::anonymous`.

An active `AgentCredential` with a non-empty `expires_at` is valid only when the
field is well-formed RFC3339 and strictly later than the injected current time.
Malformed and expired values fail closed. Successful identity resolutions are
not cached: every protected request validates the credential, validates its
linked `AgentType`, then re-reads the credential and requires the same sequence,
status, and fields. That stability check prevents a mixed-time identity assembled
from two authority states that were never active together. When an event journal is configured, resolution ignores
snapshots and strictly replays each complete durable journal; read failures,
sequence gaps, malformed or actor-misbound events, envelope/payload action
mismatches, transitions incompatible with the active spec, contradictory
tombstones, and history after a terminal tombstone deny the request. In-memory
deployments read their single local actor state. This makes credential
revocation, generic credential deletion, and AgentType deprecation effective on
the next request, including when another replica performed the durable mutation.

This deliberately adds three authoritative state reads to each protected request.
For a persistent deployment those reads are full journal replays, each bounded
by the existing 10,000-event replay budget. Identity journals are expected to be
short, but the added latency is accepted in exchange for eliminating a
revocation window. A later optimization must provide a durable, replica-visible
version or revocation generation with equivalent fail-closed semantics; a TTL
positive cache is not compatible with this boundary.

Tenant authorization also fails closed when no tenant policy set is active.
`ServerState` starts with the default-deny engine, and
`authorize_for_tenant` never falls back to a process-global compatibility
policy. A missing, corrupt, or failed policy load therefore denies requests
until that exact tenant's last-known-good policy is restored; it cannot become
permit-all or borrow another scope (ARN-230).

Cedar resource identity is server-derived as well. Shared resource builders
copy ordinary domain fields first, discard the reserved `id`/`Id`,
`status`/`Status`, `has_spec`, and `ctx_*_status` namespace, then install the
canonical entity ID, spec-defined lifecycle state, governance bit, and resolved
context-entity statuses. Collection creates reject conflicting identity aliases
and any caller lifecycle value that differs from the spec initial state. The
runtime publishes both `id`/`Id` and `status`/`Status` for compatibility with
existing lowercase and OData-style schemas, but strips all caller values first
and derives every alias from the same entity ID and lifecycle state.
Content-addressed creates use the same builder with their predeclared digest ID.
A caller cannot therefore select a permitted Cedar resource while persisting or
targeting a different entity.

The reserved-field contract is shared with the spec parser and entity mutation
boundary. Specs cannot declare runtime-owned field names as state variables or
action parameters. Runtime action parameters, direct field updates, initial
state, snapshots, and replay are sanitized through one helper before they can
become fields or durable event parameters; identity and lifecycle are then
reinstalled from `EntityState`. This protects non-OData ingress and legacy
persisted input without maintaining a second local denylist.

Bound actions, PATCH, PUT, and DELETE bind each Cedar decision to a canonical digest of
the exact local actor sequence, lifecycle status, and fields that were
authorized. The actor compares that digest inside its mailbox immediately
before mutation. A stale decision returns 409 and changes nothing. Including
fields as well as the durable sequence is required because in-memory/no-journal
actors can mutate while their sequence remains zero. Field writes use one ask
attempt: retrying the same compare-and-set after a lost reply could misreport a
committed write as a conflict, so clients must re-read and re-authorize. Actions
retain their idempotency key; a retry of an already-applied action returns the
cached/durable result before evaluating the now-stale digest.

That local compare-and-set does not by itself prove a Cedar `ctx_*_status`
derived from another entity stream. A second read or a process-local mutex would
still race across replicas. Context-dependent authorization therefore requires
the ARN-192 event-store primitive to atomically compare the referenced stream
sequences and append the target field event in one backend transaction. PR #343
must remain open until that guarded append is integrated; this ADR explicitly
rejects shipping a local-only check as a complete cross-entity fix.

### 3. The kernel router enforces the boundary itself

`build_router` installs the authority-header strip and a protected-route guard,
so an embedder cannot accidentally expose handlers that trust raw headers.
Protected handlers require `AuthenticatedRequestContext`. Tests and trusted
in-process callers install an explicit typed context; they do not forge HTTP
headers. Webhook ingress is governed by its separate Class B admission boundary
and static/liveness routes remain deliberately public.

`$hints` is not static service metadata. Hints can be enriched from runtime
trajectory analysis, so the endpoint requires a typed credential and reads a
tenant-keyed bounded map. One tenant cannot observe another tenant's learned
operational guidance.

Tenant selection after authentication comes from the typed context. A caller's
`x-tenant-id` is only an input to credential resolution and must equal the
context tenant downstream.

### 4. Internal WASM calls use capabilities, never the root key

The local-TData optimization passes the invocation's typed context and fixed
tenant directly to OData. Guest-supplied authority and tenant headers are
discarded. The guest may supply ordinary content, tracing, and idempotency
headers only.

HTTP fallthrough that must re-enter the server uses a server-issued internal
invocation credential with all of these properties:

- opaque, high-entropy, and stored only as a digest;
- bound to the exact tenant, HTTP method, canonical path/query, and typed
  `SecurityContext`;
- short-lived and single-use;
- held in a bounded `BTreeMap` with deterministic eviction order and explicit
  expiry;
- consumed by the normal bearer edge before any handler runs.

Only the server-owned canonical API base URL classifies a destination as
internal. Tenant secrets and integration configuration cannot reclassify an
external origin and cause a capability to be sent there.

The local blob fast path has a separate, narrower capability. It is constructed
only after Cedar authorizes the tenant's `blob_endpoint` bootstrap secret, and
it binds the exact parsed loopback scheme, host, explicit port, and
`/_internal/blobs` path. Only `GET` and `PUT` below that exact path are exempted
from the ordinary outbound-HTTP policy. Other ports, userinfo, query strings,
path substrings, and sibling paths fall through to Cedar default-deny. The fast
path and its authorization gate share the same parsed endpoint type so their
classifiers cannot drift.

Issuance uses an injected cryptographic token source in production and an
injected seeded source in simulation. Redirects are disabled for capability
requests. A wrong tenant, method, path, replay, or expired credential returns
401 and never falls back to another bearer interpretation.

`SecurityContext::system()` is not capability-delegable. Both issuance and
consumption reject it, because an opaque lookup would still reconstitute System
authority across an HTTP bearer boundary. System-owned work must use a direct
typed kernel API.

`ProductionWasmHost` no longer reads or receives `TEMPER_API_KEY`, and the root
key is not exposed as a WASM secret. Native adapters declare whether they need a
tenant-scoped platform credential; only the Claude Code and Codex CLI adapters
do. Before either child starts, `env_clear` removes the complete server process
environment. A small non-authority runtime allowlist (`PATH`, temporary/locale/
terminal values, and certificate-bundle paths) is copied back. Provider
credentials come only from the current tenant's named secrets and only for the
selected adapter; `HOME`, `CODEX_HOME`, `CLAUDE_CONFIG_DIR`, and provider base
URLs must be explicit integration configuration. The newly minted invocation
credential is then installed. Database, Turso, deployment, webhook, proxy,
unrelated-provider, and arbitrary tenant secrets cannot cross by ambient
inheritance. If required credential or explicit CLI configuration is absent,
the child fails normally instead of recovering ambient server authority.

CLI stdin is closed and stdout/stderr are captured through one shared bounded
runner, not `Command::output`. Each stream has a 4 MiB retention budget. Both
pipes are read concurrently; crossing either budget immediately kills and reaps
the child and fails the invocation, preventing an adapter process from growing
the server heap without bound.

Adapter credentials have a deterministic 61-minute expiry derived from
`sim_now()`, paired with a 60-minute invocation budget. One detached task owns
both adapter execution and cleanup, so cancellation of the request awaiting it
does not cancel cleanup. It catches adapter errors and panics, terminates CLI
children on timeout, and applies the normal spec-governed `Revoke` transition
through the entity actor with a stable idempotency key and three bounded retry
attempts before returning. The durable expiry is the fail-closed backstop for a
process-wide crash that prevents cleanup from running.

Minting is itself Cedar-authorized as `Issue` on the prospective
`AgentCredential`, using the original invocation security context and resource
attributes that include the target `agent_type_id`, instance ID, expiry, and
server-generated credential ID. Permission to run the source entity action is
not treated as implicit permission to mint any AgentType identity. Missing
authority context or a denied delegation aborts before an `AgentCredential`
actor is created.

Credential plaintext is a domain-separated SHA-256 derivation over two
independent scheduler-provided UUIDv7 values. This retains deterministic DST
injection while exceeding the 128-bit entropy bar and hiding UUID timestamp and
version structure from the bearer representation.

The plaintext credential exists only in the invocation context and child
environment: serde omits it, debug formatting exposes only a presence bit, and
adapter results/errors are recursively scrubbed of both the invocation bearer
and every tenant secret before they can be logged, returned, or persisted as
callback parameters. Longer secret values are scrubbed first so overlapping
values cannot leave a suffix. Durable credential events carry only the digest,
non-secret prefix, and expiry.

The spec-verification subprocess receives an empty environment and has
kill-on-drop enabled before it parses untrusted IOA input. Its 30-second timeout
therefore terminates the child instead of leaving an orphan holding inherited
server secrets.

### 5. Authorization consumes the resolved context unchanged

OData, Observe, API, REPL, OTS, and tenant-access middleware all use the same
typed context. Session/workflow metadata may enrich tracing, but cannot replace
the principal, tenant, scopes, role, agent type, verification status, or action
context. In-process `SecurityContext::system()` remains available only to code
that directly calls a typed kernel API; it is never serializable into HTTP
authority.

Management routes authorize the exact operation and resource rather than
hard-coding a network-unreachable principal kind:

- admission and profiling use `manage_admission` on
  `AdmissionControl::<tenant/entity>` and `capture_profile` on the exact
  `Profiler` mode;
- REPL execution uses `execute_repl` on `Repl::<tenant>`;
- server-local directory loading requires a distinct
  `load_specs_from_directory` permission on the canonical
  `SpecDirectory::<path>`, after the credential tenant matches the target;
- tenant membership and lifecycle routes use exact Tenant/TenantUser resources,
  while deployment-wide create/list operations additionally require a
  default-tenant control-plane credential; and
- Genesis installation binds the body target to the credential tenant and
  authorizes the pinned `App` resource before materialization. Follow-update
  reads are authorization-gated and filtered to that tenant.

The duplicate Observe tenant-deletion route is removed. Shared helpers perform
these checks; there is no second header- or route-specific identity model.

Server-local spec directories are canonicalized and must be real directories,
not symlinks. Model, invariant, and IOA inputs must be regular files and are
subject to per-file, file-count, directory-entry, and aggregate-byte budgets.
Inline specs stage in a unique process-private `TempDir`; every filename is a
normalized relative path below the one exact `model.csdl.xml` directory.
Absolute paths, `.`/`..`, suffix-confused model names, sibling roots, and
oversized components fail before a host write. Non-empty bundled Cedar policy
text is rejected and must use the separately authorized policy API.

### 6. Tenant ownership is enforced by durable evolution APIs

Authentication at a handler is not data isolation. Every trajectory analysis,
feature request, evolution record, record-chain traversal, and live evolution
event carries the credential-bound tenant into the durable store operation.
Shared Postgres tables include `tenant` in every predicate and write. Turso
tables retain the tenant column even when a database is tenant-routed, so a
misrouted store cannot turn into cross-tenant access. Filters are applied in SQL
before ordering or limiting; handlers never fetch a global result and postfilter
it.

Existing Turso databases receive an idempotent tenant-column migration. Legacy
rows are assigned to `default`, preserving their historical ownership without
making them visible to newly created tenants. Evolution broadcasts retain the
tenant on the event and subscribers filter before serialization.

### 7. Governance approval is durable before it becomes authority

Approving a pending authorization decision follows one ordered state change:

1. validate the candidate combined Cedar policy without activating it;
2. durably persist the policy and approved decision;
3. activate the exact persisted candidate in the tenant engine.

Persistence errors are returned to the caller and never collapsed into a
best-effort boolean. If runtime activation fails after durable writes, the
operation compensates both durable records to their previous values before
returning an error. A decision is never reported approved while its policy is
absent, and a policy is never left active while its decision remains pending.

### 8. Raw content-addressed ingest admits the resource before reading bytes

`POST /tdata/Blobs/Temper.IngestRaw` preserves the existing 2 GiB object
contract without materializing the raw body, canonical bytes, or either base64
field on the heap. The client must supply `X-Expected-Object-Id`, the lowercase
40-character SHA-1 of `blob <Content-Length>\0<body>`. Requiring the expected ID
is a deliberate protocol change: without it, a content-derived resource ID is
unknowable until after an attacker-controlled body has already been consumed,
so exact Cedar and quota admission cannot happen at the security boundary.

Before polling the request body, the handler:

1. validates the typed principal, tenant, repository ID, expected object ID,
   and declared length;
2. validates the declaration against the object and process staging capacities,
   then acquires one tenant-fair upload slot, one bounded global concurrency
   slot, and one fixed staging unit;
3. verifies Cedar create authority for that exact Blob ID, repository, and
   declared size;
4. runs repository and account admission, then reserves the exact owner bytes
   in the shared commons storage-cap ledger; and
5. verifies that a durable object-store backend and staging path are available.

The body is then copied through the shared blob-store I/O boundary into a
temporary staging object while its canonical SHA-1 is computed. Short, long,
failed, cancelled, and wrong-digest streams delete their staging object and
cannot create an entity. The staging budget is injected into `ServerState` and
uses deterministic defaults. Its byte permits grow in fixed units with bytes
actually accepted by the staging file, not with the attacker-controlled
declaration. Aggregate staged bytes therefore cannot exceed the configured
capacity, while a stalled 2 GiB declaration holds one staging unit rather than
reserving gigabytes. An upload that would cross the actual-byte budget fails
and releases its temporary file and permits.

The owner-byte reservation is RAII and all ordinary commons writes include
pending reservations in their cap calculation. The coarse commons mutation
lock is held only while a reservation is created and while final metadata is
persisted, never while an attacker-controlled body or remote object-store
request is pending. Cancellation removes the reservation; successful metadata
persistence clears the storage projection cache and releases the reservation
before unlocking final publication.

Raw-upload admission is tenant-fair rather than a single global slowloris
queue. A tenant may occupy at most one raw-upload slot at a time, while a
separate bounded global concurrency budget allows other tenants to progress.
The tenant-slot registry itself is bounded and deterministic. Every slot,
incremental staging reservation, and owner-byte reservation is RAII, including
when the request future is cancelled.

The staging copy has three independent progress bounds: a maximum interval
without a non-empty body chunk, a total upload deadline, and a minimum average
throughput after a short grace period. A client cannot keep a global upload
slot indefinitely or grow its actual-byte staging reservation by stalling or
sending one byte just before the idle deadline. Independent total-operation
boundaries cover staging and each subsequent local or S3 object-store stream.
S3 clients use explicit connect and request timeouts; local staging, flush,
sync, and publish operations use bounded async I/O waits. These are production
I/O deadlines and do not enter deterministic simulation state.

After digest verification, the blob-store boundary reads staging in bounded
chunks and streams two standard field-overflow JSON representations: base64
content and base64 canonical bytes. Their keys use the existing
`field-overflow/sha256/*.json` namespace and their entity fields use the
existing `__temper_blob_ref` envelope. This keeps OData and Genesis hydration
on one representation instead of introducing a second raw-blob schema. Both
overflow objects must be durable before the small metadata entity is created.
Content-addressed orphan objects after a later failure are harmless and may be
swept; a partially created Blob entity is not permitted.

The create response is metadata-only for the two binary fields. Hydrating
multi-gigabyte fields merely to echo an accepted upload would recreate the
memory-amplification vulnerability. Existing read paths remain the explicit
place to request field content.

### 9. Blob hydration is aggregate-bounded and large fields are media streams

An overflow envelope is a media descriptor, not permission to allocate its
declared size. Generic OData entity/list/expand reads, Observe reads, query
materialization, and action responses share a 1 MiB aggregate inline hydration
budget for the complete response. No individual overflow value above the
normal 128 KiB inline ceiling is fetched for those JSON responses. Once either
budget is exhausted, the original bounded `__temper_blob_ref`, size, and
encoding descriptor remains in the response. Blob-store reads check the
authoritative object size before buffering, so a forged or stale small size in
an envelope cannot bypass the budget. A shared 64-attempt I/O budget also
bounds missing, corrupt, or repeatedly referenced objects that consume no byte
budget; failed keys are remembered for the response and are not retried.

Blob-store resolution adds a deterministic tenant namespace below every
non-default local root and remote bucket prefix. A descriptor copied or guessed
from another tenant therefore resolves in the caller's namespace, not the
source tenant's. The historical database blob table has no tenant column, so
its legacy fallback and shadow writes are restricted to the `default` tenant;
non-default tenants never fall back into that global keyspace.

The compatibility `/_internal/blobs/{key}` transport also consumes
`AuthenticatedRequestContext` and selects that exact credential tenant. It
never substitutes `default`, and it requires `read_blob_object` or
`write_blob_object` on the exact `BlobObject::<key>` before touching storage.
This closes the overlap with ARN-219's file helper authorization audit and
ARN-233's tenant-to-storage-key audit without claiming those broader sink
reviews are complete.

WASM dispatch uses separate hard aggregate budgets for both inline hydration
and its deferred cache. The deferred map is never an unbounded alternate heap:
an entry that exceeds its individual limit or the invocation's remaining
aggregate cache budget stays a descriptor and is not fetched. These limits are
below the default guest memory budget and are enforced before the invocation
thread is created.

Large Git Blob fields remain fully available through authenticated primitive
media endpoints:

- `GET /tdata/Blobs('<id>')/Content/$value` streams the decoded raw blob body.
- `GET /tdata/Blobs('<id>')/CanonicalBytes/$value` streams the decoded
  `blob <size>\0<body>` representation.

Both paths require the same typed request context and exact Blob read
authorization as the entity endpoint. They load and authorize only bounded
metadata, validate that the requested property is one of the two supported
binary fields, require a canonical SHA-256 field-overflow key and JSON encoding,
check both encoded and decoded lengths, and verify the serialized object
against its content-addressed key while forwarding bounded chunks from the
object store through an incremental JSON-string/base64 decoder. A separate
stream-concurrency budget prevents slow media consumers from consuming the
ordinary blob-I/O budget. The paths never build a JSON string or a decoded
body-sized `Vec`.

Genesis materialization consumes that same representation incrementally.
Blob `Content` is decoded directly into an RAII temporary file and atomically
published at the destination. App identity components are validated before
joining cache paths, cache-root names include a digest of the complete pinned
reference, and complete trees are staged beside the destination before a
rollback-safe directory replacement. Remote bundle bodies, manifests, file
counts, individual files, and aggregate exported bytes have explicit budgets;
bundle traversal rejects symbolic links. Before object reads or file creation,
tree materialization now consumes closure-wide app, tree-object, tree-entry,
depth, canonical-tree-byte, file-count, per-file, and aggregate-file-byte
budgets. Conflicting versions of one dependency and different owners that map
to the same cache directory fail closed instead of selecting traversal order.
The materializer no longer performs
`Vec -> serde_json::Value/String -> base64 Vec` for large Blob content, and a
failed, truncated, malformed, or over-budget stream cannot leave a partially
published application file.

Pinned Git identity is not yet independently recomputed across Commit, Tree,
and Blob objects at this boundary. That remains a release blocker for treating
the Genesis registry itself as hostile rather than an authenticated registry.

### 10. Buffered text batches have positional and byte budgets

The authenticated file-text batch endpoints remain buffered JSON APIs, so they
accept at most 100 distinct, non-empty identifiers. Duplicate identifiers are
rejected instead of being authorized once and fetched repeatedly. Each text
item is limited to the same 2 MiB boundary as the public buffered File
`$value` write path, and the aggregate text response is limited to 16 MiB.
Larger files use the authenticated streaming `$value` endpoint. The state-layer
reader enforces the same contract as the HTTP handler and consumes the response
budget before retaining each result, so an internal caller cannot bypass it.

### 11. Fresh audit findings not falsely claimed as closed

The admission endpoint's authentication is corrected here, but its runtime
override is not effective. `override_caps(entity)` writes a process-global
controller entry, while dispatch always re-reads tenant spec caps and passes
them to `try_acquire_with_caps`; that call replaces the registered entry. The
endpoint can therefore report success without changing effective admission.
An ignored red regression records the required precedence. A separate ARN must
redesign overrides as tenant-keyed dispatch inputs; this ADR does not describe
the current no-op as fixed.

The same fresh audit also found a broader exact-row authorization gap in
Observe handlers that pass no resource ID to Cedar before fetching a selected
entity/spec/module/record, plus process-global health/metrics/refresh surfaces
that cannot be made tenant-safe until the underlying metrics state is tenant
dimensioned. Those are ARN-219-class sink audits and require dedicated lanes;
the credential edge and management routes fixed here do not imply those
broader surfaces are complete.

## Consequences

### Positive

- One credential resolution determines both tenant and principal.
- Raw headers, loopback origin, and a shared root secret cannot become authority.
- Local and network WASM calls preserve least privilege without duplicate auth
  implementations.
- Direct router embeds fail safe by default.
- Repeated batch identifiers cannot multiply one authorized blob read into an
  unbounded buffered response.
- The same foundation can be reused by ARN-166, ARN-167, and ARN-187.

### Negative

- Existing tests and internal callers that asserted principal headers must move
  to typed contexts.
- Operator access must be registered per tenant instead of implicitly spanning
  the deployment.
- Internal HTTP re-entry requires a bounded credential store and explicit token
  source rather than one environment-variable lookup.
- Raw Blob clients must compute and send the canonical object ID before upload.
  The server performs extra sequential disk reads to keep memory bounded, and
  the configured staging-byte budget limits aggregate actual in-flight disk use
  in fixed-size accounting units.
- Large Blob fields remain descriptors in JSON reads. Clients that need their
  bytes must use the authenticated property `$value` media endpoint.
- A tenant can run only one raw ingest at a time. This is deliberate fair-share
  admission; independent tenants retain progress even when one sender stalls.
- Protected requests perform two authoritative identity-state reads. Persistent
  deployments pay bounded full-journal replay latency until a durable,
  replica-visible version primitive is available.
- Native adapter invocations have a one-hour execution budget. Work that needs
  more time must checkpoint and resume under a newly minted credential.

## Verification

The change is accepted only with end-to-end tests proving:

- forged principal, scope, role, ABAC, action-context, and tenant headers do not
  affect the typed principal;
- missing, malformed, wrong-tenant, expired, replayed, wrong-method, and
  wrong-path credentials are rejected before persistence;
- an already-used resolver rejects a directly revoked credential and a
  credential linked to a newly deprecated AgentType on its next call; generic
  OData deletion cannot retain authority, and a second `ServerState` sharing the
  durable store observes a revocation without process-local invalidation;
- the no-key network server rejects protected reads and writes;
- the exported kernel router rejects protected requests without a typed context;
- `$hints` rejects anonymous requests and returns only the authenticated
  tenant's bounded hint set;
- local TData and real `ProductionWasmHost` fallthrough preserve the exact
  invocation principal and tenant;
- internal blob writes and reads remain isolated for identical keys in two
  tenants, sibling keys require separate Cedar authority, and near-match
  loopback origins/paths receive no blob capability;
- admin, REPL, tenant-management, user-membership, and Genesis installation
  routes reject missing, wrong-tenant, wrong-resource, and policy-less typed
  principals before mutation; the duplicate Observe deletion path is absent;
- server-local spec loading rejects cross-tenant credentials, unapproved
  canonical directories, symlinked files/directories, and all configured
  count/byte budgets before registration; inline paths cannot escape their
  unique staging directory and bundled Cedar text cannot become authority;
- the deployment key is never selected as an Agent/Admin fallback; and
- CLI adapter children never inherit the deployment key; captured invocation
  credentials are rejected by a second `ServerState` immediately after success,
  adapter error, and caller cancellation, and are rejected after deterministic
  expiry even when cleanup does not run; plaintext is absent from serialized
  contexts, returned results/errors, debug output, and durable journal events;
  credential-shape tests prove both independent UUID sources contribute to an
  opaque 256-bit digest representation; real child-process sentinel tests prove
  database, Turso, webhook, cloud, deployment-root, unrelated-provider, and
  arbitrary tenant secrets are absent while the selected tenant provider key,
  explicit config paths, runtime `PATH`, and invocation bearer remain;
- the isolated verifier child receives no parent environment and is killed when
  its execution budget expires;
- evolution list/get/update/chain/stream operations cannot observe another
  tenant even when record identifiers collide;
- failed policy or decision persistence leaves runtime authority unchanged, and
  failed activation restores the prior durable policy and decision; and
- raw Blob authorization, storage-cap rejection, and staging-budget rejection
  do not poll the body; wrong digests, short/long streams, cancellation, and
  object-store failures leave no entity or staging file; concurrent uploads
  cannot exceed the actual staged-byte budget; a stalled or trickled upload loses
  all admission reservations at its progress/deadline bound, and another tenant
  can upload while it is in flight;
- collection and content-addressed creates overwrite all trusted Cedar aliases
  from server state, reject conflicting `id`/`Id` and lifecycle values, and
  never persist caller-supplied context-status or governance attributes;
  action events, direct PATCH/PUT field updates, initial state, and restored
  snapshots likewise retain only server-derived, mutually equal
  `id`/`Id`/`status`/`Status` aliases, and specs declaring those reserved names
  fail validation;
- generic entity/list/expand/Observe reads never inline more than the aggregate
  hydration budget, a lying overflow size cannot cause an oversized read, and
  WASM deferred caches remain below their hard aggregate budget;
- both Blob property `$value` endpoints reject unauthenticated/unauthorized
  callers and stream multi-chunk decoded bytes without hydrating the entity;
- Genesis admits dependency closure, tree traversal, and output bytes against
  aggregate budgets before materialization; it writes large Blob content
  directly to a temporary file with bounded memory and removes the temporary
  output on malformed or failed input;
  and
- legitimate tenant credentials, workflow correlation, and public health checks
  continue to work.

## DST Compliance

The identity resolver retains no positive authority state and uses injected
simulation time for expiry checks. Strict durable replay consumes events in
sequence order under the existing replay budget. The internal single-use
capability store uses `BTreeMap`, bounded capacity, injected time, and an injected
token source. Simulation never calls OS randomness or wall-clock time. Local
dispatch continues through the production OData authorization path with a typed
context rather than a parallel test implementation. Adapter credential expiry
also uses `sim_now`; the detached task and wall-clock timeout wrap external CLI
side effects only, outside simulated transition semantics. Revocation still
uses the production entity actor and event journal path.

## Rejected Alternatives

1. **Strip headers only.** In-process and future direct-embed paths do not
   necessarily cross that strip layer.
2. **A trusted header marker.** It merely creates a second forgeable authority
   input and requires perfect middleware ordering.
3. **Treat no-key mode as anonymous Customer.** Shipped policies give anonymous
   principals useful capabilities, so this is not fail-closed.
4. **Continue passing the root API key to WASM.** Tenant checks after a
   deployment-wide Admin credential cannot recover least privilege.
5. **Sign forwarded identity headers with one long-lived HMAC key.** This still
   serializes authority into replayable bearer data and duplicates credential
   lifecycle logic; bounded single-use capabilities are narrower and auditable.
6. **Keep full hydration but rely on the 2 GiB object limit.** One object can
   expand into several body-sized allocations, and a list or `$expand` can
   multiply that again. An aggregate response budget must precede storage I/O.
7. **Make only the upload idle timeout finite.** A one-byte trickle can satisfy
   an idle timer forever. Idle, total, and sustained-throughput bounds are all
   required, together with per-tenant fair-share admission.
8. **Keep a short identity TTL plus mutation-path invalidation.** Any positive
   TTL extends authority after a missed mutation. HTTP middleware cannot observe
   internal actions, generic entity mutations, or writes performed by another
   replica, so enumerating invalidation paths is not an authority boundary.
9. **Let adapter credentials inherit the process key or remain permanent.** An
   inherited root credential destroys tenant least privilege; a permanent child
   credential turns one invocation into durable authority. Explicit environment
   removal, bounded minting, durable revocation, and expiry are all required.
