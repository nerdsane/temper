# ADR-0188: Durable Stream Descriptor Authority

- Status: Accepted
- Date: 2026-08-26
- Deciders: Temper core maintainers
- Related:
  - ADR-0029: Temper Filesystem
  - ADR-0057: Native Immutable File Read Plane
  - ADR-0063: Object Store for Blob Bytes
  - ADR-0088: Native File `$value` Write Fast Path
  - ADR-0097: Overlap File Blob Write and State Read
  - ADR-0157: Metadata-Generated Typed Module Data SDK
  - GitHub issue #67: Typed File streaming ignores PawFS `SizeBytes` length
  - `crates/temper-server/src/application_data/streams.rs`
  - `crates/temper-server/src/state/file_reads.rs`
  - `crates/temper-server/src/state/file_read_projection.rs`
  - `crates/temper-server/src/state/file_writes.rs`
  - `os-apps/temper-fs/specs/`

## Context

ADR-0029 separates queryable File metadata from binary content and makes
`HasStream=true` the generic CSDL declaration for an entity with an OData
`$value` stream. ADR-0088 gives the built-in File path a native byte-storage
implementation while preserving verified `StreamUpdated` dispatch as the model
commit boundary. ADR-0157 adds generated typed File clients whose content reads
must enforce an artifact-bound `max_stream_bytes` budget before opening a host
stream.

The typed read implementation currently tries to obtain the pre-read length by
searching entity fields for several spellings:

```text
Size
size
ContentLength
content_length
```

TemperFS declares the public CSDL property as `SizeBytes`, while its verified
IOA state variable is `size_bytes`. After durable replay, typed
`FileClient::open_file_read` therefore returns `FileLengthUnavailable` even
though the supported `$value` path committed a bounded length. Version reads
have the same class of defect for ownership: the host checks `FileId`, while
the verified FileVersion state persists `file_id`.

Adding `SizeBytes` and `size_bytes` to these lists would repair the observed
fixture but would preserve the wrong abstraction. A property-name list asks
kernel code to infer semantic authority from application vocabulary. It allows
two schemas with identical scalar shapes but different meanings to be treated
as equivalent, makes safety behavior depend on casing conventions, and
contradicts ADR-0157's rule that runtime code does not guess names. It also
leaves every future stream-capable entity to discover the same hidden naming
contract.

The byte length used for admission is not merely display metadata. It is a
security and resource-control fact: the host must reject content larger than
the artifact-bound stream budget before fetching or allocating the content.
The authority for that fact must therefore be created by the platform at the
byte commit boundary, persisted durably with commit provenance, and restored
without consulting application field names.

## Decision

Temper will make a platform-owned, durable stream descriptor the authority for
stream identity and pre-read admission. Entity properties such as `SizeBytes`,
`ContentHash`, and `MimeType` remain application state and projections. They
may mirror and be checked against the descriptor, but the kernel will not infer
stream safety metadata or ownership from their names.

### Sub-Decision 1: Use One Typed Descriptor for Current and Immutable Streams

The kernel contract will have a versioned shape equivalent to:

```text
StreamDescriptorV1 {
    subject: EntityRef { entity_type, entity_id },
    authorization_parent: EntityRef?,
    content_hash: String,
    storage: StreamStorageRefV1,
    byte_length: u64,
    content_type: String?,
    content_event_sequence: u64,
    descriptor_event_sequence: u64,
    mutability: Mutable | Immutable,
}
```

The tenant is supplied by the storage and invocation authority and is not a
caller-controlled field. `subject` identifies the entity whose `$value` is
being opened. `authorization_parent` is absent when the stream authorizes
directly against its subject and identifies the parent resource when the
verified stream capability declares parent-scoped authorization.
`content_event_sequence` identifies the domain event that published the
content; `descriptor_event_sequence` identifies the event envelope that first
recorded or later backfilled the descriptor. They are equal for new commits and
may differ only for an audited migration record. The descriptor has a closed
versioned encoding, rejects unknown fields, and uses bounded strings.

`storage` is a bounded, versioned, kernel-only reference containing the blob
storage contract version and opaque object identity returned by the configured
platform `BlobStore`. It is persisted because an invocation-local receipt no
longer exists during replay or a later read. It is never returned to a guest or
accepted from application data. `content_hash` remains an integrity claim and
does not implicitly double as a provider key.

The built-in current File descriptor is replaced only by a later verified
content commit. A FileVersion descriptor is immutable after its creation. A
version read authorizes the requested File, then requires the version
descriptor's typed authorization parent to equal that File. It does not inspect
a `FileId`-like property.

`authorization_parent` and `mutability` are generic stream-capability semantics,
not implicit TemperFS behavior. They come from a closed CSDL stream vocabulary,
not from names or ambient runtime discovery. The canonical declarations are:

```xml
<!-- On the current stream entity. -->
<Annotation Term="Temper.Vocab.Stream.Mutability" String="Mutable"/>
<Annotation Term="Temper.Vocab.Stream.VersionEntityType"
            String="Temper.FS.FileVersion"/>
<Annotation Term="Temper.Vocab.Stream.VersionCollection"
            NavigationPropertyPath="Versions"/>

<!-- On the immutable version entity. -->
<Annotation Term="Temper.Vocab.Stream.Mutability" String="Immutable"/>
<Annotation Term="Temper.Vocab.Stream.AuthorizationParent"
            NavigationPropertyPath="File"/>
```

The parser will support the closed `NavigationPropertyPath` annotation value.
Verification resolves every path through canonical CSDL navigation metadata,
requires the version collection target and authorization-parent target to be
mutual and type-correct, validates the referential constraints, and rejects
unknown mutability values or incomplete contracts. A version entity need not
independently expose public OData `$value`; its content capability is reached
through the verified current entity's version contract.

Stream semantics are deliberately inert during the reader-first rollout. A
separate, closed per-entity activation marker enables descriptor writes and
strict reads only in a later pinned tenant schema:

```xml
<Annotation Term="Temper.Vocab.Stream.DescriptorContractVersion" Int="1"/>
```

Verification rejects unsupported versions. Deployment may add this marker only
after descriptor-aware writer/replay fleet checks and the tenant's durable
migration inventory are complete. Removing it disables strict opens and new
descriptor writes but does not permit descriptor metadata to be discarded.

Verification emits a canonical `StreamCapabilityV1` into the locked application
closure and module SDK manifest. It contains fully qualified subject and version
types, mutability, canonical navigation identities, and authorization-parent
type. Its semantic digest is part of the artifact binding. For TemperFS, File
declares a mutable direct stream and FileVersion declares an immutable stream
authorized through its verified File relationship. Publication rejects a
descriptor whose relation or mutability does not match this bound capability.

**Why this approach**: length, content identity, ownership, and commit sequence
are one consistency claim. Keeping them together prevents the host from
combining a length from one entity version with bytes or ownership from
another.

### Sub-Decision 2: Commit the Descriptor as Kernel Event Metadata

The `$value` write path receives a host-attested storage receipt containing the
actual accepted byte count, platform digest, content type, and opaque storage
identity before publishing the verified entity transition. It will pass a typed
pending descriptor through the dispatch context. The successful event commit
will include the descriptor in an optional, reserved, version-tagged kernel
metadata envelope alongside the ordinary IOA event data.

The descriptor is not an IOA field, action parameter, integration value, or
guest-supplied JSON property. Application effects cannot set or patch it. The
kernel validates that:

- the descriptor subject matches the dispatched entity;
- `byte_length` equals the host-attested receipt's accepted byte count;
- `content_hash` equals the host-attested receipt's platform digest;
- `storage` equals the host-attested receipt's bounded storage reference;
- the entity has a verified stream capability;
- the descriptor relation and mutability match that capability;
- a normal commit has equal content and descriptor event sequences, both equal
  to the sequence assigned to the committed event; and
- an immutable descriptor is not replacing a prior immutable descriptor.

FileVersion creation receives the descriptor through typed trigger provenance,
not through copied field names. The reaction may still populate application
metadata fields for OData and domain logic, but the target commit attaches an
immutable descriptor whose authorization parent is the source File.

Event append and descriptor provenance are one per-entity durable record.
Projection into a query store or cache may lag without losing authority.
Replay reconstructs the same descriptor deterministically from the journal.

The event metadata member is optional for backward decoding: historical events
decode as `None` and replay exactly as they do today. A descriptor-aware binary
can run with strict admission disabled while migration is incomplete. New
descriptor-bearing events are not enabled until every writer, replay worker,
and rollback candidate understands and preserves the version-tagged envelope.
Older binaries are not permitted to consume or rewrite descriptor-bearing
journals; the deployment gate prevents mixed-version writes rather than relying
on unknown-field tolerance.

**Why this approach**: a separate best-effort descriptor write would create a
crash window in which entity state advertises new content while admission
metadata is absent or stale. Kernel event metadata preserves the verified
transition as the publication boundary established by ADR-0088.

### Sub-Decision 3: Require a Host-Attested Storage Receipt

Only the platform blob boundary can mint `CommittedStreamReceiptV1`. The receipt
is an invocation-local, opaque host handle backed by bounded host state; guest
memory cannot construct or deserialize one into authority. It records the byte
count and platform digest computed by the host over the accepted stream plus the
bounded `StreamStorageRefV1` needed by the configured `BlobStore`
implementation. Descriptor commit copies that reference into durable kernel
metadata before consuming the receipt.

The native File path receives the receipt directly from the existing
content-addressed blob write. A WASM blob adapter may still choose application
policy, caching, or when to request storage, but a typed stream commit must end
in a host blob-store operation that returns this receipt. An arbitrary
`host_http_call_stream` success or guest callback containing a hash, URL, size,
or provider response cannot publish a descriptor.

The built-in File fallback described by ADR-0088 will migrate to the attested
receipt path before strict activation. Environments whose adapters cannot use
the host blob-store receipt keep the prior deployment; they do not receive a
runtime compatibility bypass. Non-typed integration streams may continue to
use generic outbound HTTP, but they cannot claim the typed File capability.

**Why this approach**: the host can attest bytes it accepted and storage it
performed. It cannot derive trustworthy blob identity from guest-selected
callback fields, even when it independently knows the input length.

### Sub-Decision 4: Separate Metadata Admission from Blob Fetch

The read plane will expose a metadata-only lookup before any blob read:

```text
resolve_stream_descriptor(tenant, subject) -> StreamDescriptorV1
open_stream_bytes(tenant, descriptor.storage) -> bounded byte stream
```

`FileReadOpen` follows this order:

1. Resolve the artifact-bound File capability and authorize the requested File.
2. Resolve the authoritative descriptor for the current File or requested
   FileVersion.
3. For a version, verify the descriptor's typed authorization parent.
4. Reject a missing, stale, corrupt, or sequence-inconsistent descriptor.
5. Compare `byte_length` with `max_stream_bytes` and reject an oversized stream.
6. Only then resolve `storage` through the configured `BlobStore`, open the blob,
   and allocate a bounded host handle.
7. Verify that the bytes delivered by the blob boundary match the descriptor's
   declared length and digest; an integrity mismatch fails closed.

Query-plane metadata may accelerate step 2 only when its descriptor sequence is
current. The authoritative replay path is the bounded fallback. Blob stores
will provide metadata/stat support where needed for integrity checks and
migration; a stat response is not allowed to widen the artifact budget.

Missing or inconsistent admission metadata returns a structured consistency or
integrity error. Only a valid descriptor whose length exceeds the grant returns
`BudgetExceeded`. A content mismatch must not be reported as a budget error.

**Why this approach**: the budget exists to prevent an oversized fetch or
allocation. Checking only after `read_file_stream_indexed` has materialized the
entire blob defeats that boundary even if the operation later returns an error.

### Sub-Decision 5: Keep Application Metadata Explicitly Non-Authoritative

TemperFS continues to expose `SizeBytes`, `ContentHash`, `MimeType`, `FileId`,
and related properties because they are useful domain data. The verified IOA
continues to maintain its corresponding state and invariants. Read projections
may include those values.

The framework will not use canonical names, generated Rust names, snake-case
conversion, case-insensitive matching, or a compatibility alias list to decide
which values govern stream admission or version ownership. Generated SDK
manifest name mappings remain valid for typed entity serialization; they do
not acquire stream semantics.

Verification will check the built-in TemperFS workflow for consistency between
the application projection and the committed descriptor. A mismatch is an
observable invariant violation and fails the typed open; it is not silently
reconciled by choosing whichever value is smaller or newer.

**Why this approach**: application metadata belongs to the model plane, while
pre-read resource admission belongs to the platform data plane. Keeping the
boundary explicit preserves Temper's ability to generate different domain
schemas without teaching the kernel their vocabulary.

### Sub-Decision 6: Migrate Historical Streams Explicitly

Existing stream subjects may predate kernel descriptors. They are migrated
before the descriptor requirement is activated for their tenant. Migration is
an explicit, idempotent platform operation driven by a verified, digest-bound
provenance mapping, not an application inventory API or permanent runtime
fallback. TemperFS Files and FileVersions are the first rollout of this generic
contract.

For each historical stream, the migration will:

1. resolve its content identity through the exact verified historical schema
   version and publication-event mapping;
2. obtain the actual byte length from bounded blob metadata/stat rather than
   trusting a historical size field;
3. verify the digest and resolve any authorization parent from historical
   relationship/event provenance;
4. append a reserved `StreamDescriptorBackfilled` kernel event whose
   `content_event_sequence` identifies the historical content publication and
   whose `descriptor_event_sequence` is the new migration event sequence; and
5. record a durable failure for missing, corrupt, or ambiguous content instead
   of manufacturing a descriptor.

Activation is gated on a complete migration inventory and the distinct
`DescriptorContractVersion=1` schema marker. Once activated, typed
reads do not fall back to historical field spellings. Tenants with unresolved
records retain the prior deployment and receive an actionable bounded report.

Replay treats the backfill event as kernel metadata rather than a domain
transition. It verifies that the referenced content event is the latest content
publication for the subject at the time of backfill. Later ordinary domain
events do not stale the descriptor; only a later content publication replaces a
mutable descriptor. A migration event cannot replace an existing immutable
descriptor or point to an event that did not publish the referenced blob.

**Why this approach**: preserving working historical content requires a real
migration. Carrying name inference indefinitely would make old accidental
conventions part of the kernel ABI and conceal corrupt records.

### Sub-Decision 7: Bind Typed File Capabilities to Descriptor Support

SDK generation continues to expose File content methods only for an entity with
the verified File capability defined by ADR-0157. Publication additionally
requires the target deployment to support the descriptor contract version used
by the generated artifact.

The artifact binding records the required stream descriptor contract version,
not property aliases. Activation recomputes support from the pinned application
closure and host ABI. A guest cannot supply a descriptor, select its version,
or downgrade the requirement in a request.

The external OData `$value` path and generated typed File path use the same
descriptor admission service. Neither transport is allowed to invent a second
source of stream truth.

**Why this approach**: capability availability, schema identity, and host
support are established before execution. Runtime field discovery would
reintroduce ambient schema dependence after artifact binding.

### Sub-Decision 8: Govern Migration Through Deployment Evidence

Historical migration is a platform deployment operation, not an application
inventory API. Temper exposes bounded start, advance, inspect, and unresolved
listing operations through the existing schema-deployment HTTP and typed WASM
surface. The host supplies tenant and principal authority. A caller supplies
only an immutable deployment target, positive budgets, and idempotency data;
it cannot supply entity identities, journal sequences, blob identities, or
descriptor facts.

The verified stream capability carries a closed migration-only provenance
mapping for the publication action, content-hash parameter, byte-length
parameter, optional content-type parameter, authorization-parent parameter,
and versioned storage-key contract. Bundle verification proves those bindings
against the canonical CSDL and IOA before the platform stages a migration
target. The canonical mapping is included in the stream-capability digest.
This mapping interprets historical journal facts; it never becomes an
application-state fallback for descriptor reads.

Migration jobs page authoritative journals in deterministic subject order,
derive candidates inside Temper, stat and hash blob bytes through bounded
storage operations, and append only verified descriptors. Jobs durably own
their cursor, latest per-subject outcomes, cumulative unresolved set, page
receipts, budgets, and completion receipt. Re-running a repaired subject may
replace an unresolved outcome, while an exact request replay returns the
original receipt without duplicating a descriptor.

Completion evidence binds the tenant, deployment kind and identity, source and
target bundle digests, canonical stream-capability digest, descriptor contract
version, and the exact durable per-capability publication generations observed
by a stable complete pass. Every stream publication advances its capability's
generation in the same storage transaction as its event append. The job stores
the complete generation map rather than only a combined token. Activation uses
that stored map as its compare-and-set input, so it cannot silently rebase the
receipt onto a newer generation; a publication after inventory makes the
evidence stale without a race window.

Both task-scoped schema activation and tenant-global installed-app reconcile
enforce the same evidence contract. A target without
`DescriptorContractVersion=1` remains reader-first and requires no receipt. A
target that activates version 1 stays staged while evidence is absent, stale,
or has unresolved subjects. The storage fence rejects descriptor-less
publications after activation even if the in-memory registry has not yet
observed the new pointer. For a task cutover, the fence is selective: it blocks
only the verified descriptor-less publication action on predecessor-pinned
journals. Ordinary predecessor actions and descriptor-bearing publications
remain available.

**Why this approach**: deployment already owns verification, immutable bundle
identity, Cedar authorization, and atomic activation. Binding migration to that
boundary keeps privileged inventory in the kernel, gives application workflows
a typed progress surface, and prevents every activation path from bypassing
the same durable proof.

### Operator Contract

Operators submit the exact task-bundle or installed-application target through
`POST /api/v1/schema-deployments/stream-descriptor-migrations`, then advance it
with bounded calls to
`POST /api/v1/schema-deployments/stream-descriptor-migrations/{job_id}/advance`.
Progress is read with `GET` on the job URI; unresolved classifications are read
with `POST` to its `/unresolved` endpoint so the bounded cursor request remains
in the authenticated body. The typed WASM schema-deployment ABI exposes
the same four operations and receipts. Tenant identity always comes from the
authenticated invocation, and Cedar authorizes the concrete task scope or
installed application rather than a caller-provided surrogate.

Start and advance mutations require stable idempotency keys. Exact replays
return the originally committed receipt, including its request ID, cursor,
page outcomes, and committed sequence. Each advance is limited to 256 subjects,
1,024 events per subject, and 2 GiB of blob verification work; callers may
choose smaller positive budgets. A job stores at most 1,024 mutation receipts
and a 4 MiB state payload. Operators repair missing or corrupt blobs or invalid
publication facts at their authoritative source, then advance the same job;
the retry cursor rotates through unresolved subjects so one persistent fault
cannot starve later repairs.

Activation remains denied until a stable complete pass has no unresolved
subjects. A concurrent publication before cutover makes completion stale and
the next advance reopens inventory. Task activation and installed-app reconcile
compare and install publication fences atomically with the observed
generations. Completion evidence is a cutover prerequisite, not a perpetual
steady-state check: exact task activation replays use the durable activation
receipt, and unchanged installed-app reconcile uses the exact durable semantic,
publication-action, and capability fence. Normal post-cutover writes therefore
do not reopen migration. Rollback keeps the prior descriptor-aware deployment
active; migration records and descriptor events are append-only and are never
deleted as a rollback action.

## Rollout Plan

1. **Reader-first foundation** — PR #68 supplies descriptor-aware event
   metadata, replay, reads and writes, verified stream capabilities, and the
   inactive versioned marker. Keep the marker absent while governed migration
   evidence is unavailable.
2. **Governed migration and gate** — Issue #73 adds the verified provenance
   mapping, platform-owned bounded jobs, durable completion evidence,
   publication-generation fencing, typed progress operations, and activation
   checks for both scoped bundles and installed apps in one implementation PR.
3. **Historical migration** — Inventory every deployed stream subject, run the
   idempotent migration, and prove descriptor counts, ownership, hashes,
   lengths, and unresolved records match the inventory before activation.
4. **Activation** — Migrate TemperFS/TemperPaw bundles to artifacts bound to the
   descriptor contract and add `DescriptorContractVersion=1` only after exact
   migration-complete and fleet-ready evidence is durable. Keep the prior
   deployment live until activation and rollback gates pass.
5. **Live verification** — Exercise current and versioned typed reads after a
   real restart, verify exact content and budget rejection, and use Datadog to
   prove descriptor resolution precedes blob fetch and that no rejected open
   reads content bytes.

## Readiness Gates

- A red integration test reproduces issue #67 with the real TemperFS schema,
  supported create plus `$value` write path, durable store, process-equivalent
  restart, and generated typed `FileClient`.
- The same test reads both current content and an immutable FileVersion without
  raw HTTP fallback.
- Missing, stale, corrupt, ambiguous, or sequence-inconsistent descriptors fail
  before blob fetch with stable structured errors.
- Zero-length content succeeds; oversized content is rejected against the exact
  artifact-bound `max_stream_bytes` before blob fetch or host-stream allocation.
- Blob length or digest mismatch fails as an integrity error and cannot produce
  readable bytes.
- Version ownership comes only from the descriptor and rejects cross-File
  version IDs.
- CSDL verification rejects missing, ambiguous, mistyped, or non-mutual stream
  navigation declarations, and artifact activation detects any
  `StreamCapabilityV1` semantic drift.
- Descriptor replay and open use the exact persisted bounded storage reference;
  neither path derives a provider key from a digest or application field.
- Native and WASM-backed typed commits can publish descriptors only from a
  host-attested blob-store receipt; forged callback length, hash, URL, provider
  response, or raw HTTP success cannot mint authority.
- Historical events without kernel metadata replay unchanged. Mixed-version
  deployment tests prove descriptor writes remain disabled until every reader
  understands and preserves the version-tagged envelope.
- Fault-injection tests cover cancellation or failure before blob durability,
  between blob durability and event commit, during descriptor projection, and
  during replay. No path publishes readable content without a committed
  descriptor.
- Replay and restart tests produce byte-for-byte equivalent descriptors and do
  not depend on query-projection timing.
- Migration tests cover current Files, immutable versions, deduplicated blobs,
  missing blobs, corrupt blobs, partially migrated tenants, and idempotent
  reruns.
- A source guard rejects new stream-admission code that searches application
  field names or performs casing normalization.
- OData and typed SDK reads share the same admission result, authorization,
  content, and error classification.
- Telemetry records descriptor contract version, resolution source,
  content/descriptor event-sequence agreement, declared byte length, budget
  outcome, and whether blob fetch began, without high-cardinality content or
  identifiers.
- `cargo fmt --all -- --check`, `cargo clippy --workspace`, and
  `cargo test --workspace` pass.
- Mandatory DST and code-quality reviews pass.
- The merged change is deployed and live Datadog verification confirms the
  ordering and failure-path invariants above.

## Consequences

### Positive

- Stream budgets are enforced against platform-owned durable facts before blob
  I/O or allocation.
- Framework behavior no longer depends on TemperFS property names or casing.
- Current and versioned reads use one typed ownership and consistency contract.
- Replay, migration, and artifact compatibility become explicit and testable.
- Application schemas remain free to choose domain vocabulary.

### Negative

- Event metadata, persistence, replay, projection, trigger provenance, artifact
  binding, CSDL vocabulary, and migration all gain a new versioned contract.
- Every stream commit stores some metadata already represented in application
  state, deliberately duplicating facts across model and data planes.
- Metadata-only resolution may add a store lookup or object stat when no current
  projection is available.
- Existing deployments require inventory and migration before strict activation.

### Risks

- **Descriptor/entity drift**: application projections could disagree with the
  descriptor. Mitigation: bind content and descriptor sequences to explicit
  events, verify the TemperFS projection, emit a stable invariant failure, and
  never choose one silently.
- **Cross-actor propagation loss**: FileVersion creation could lose descriptor
  provenance. Mitigation: carry typed provenance in the durable trigger intent
  and fault-test every commit/retry boundary.
- **Unattested adapter storage**: a guest could claim bytes were stored at a
  chosen provider location. Mitigation: only an opaque receipt minted by the
  host blob-store operation can publish a typed descriptor.
- **Mixed-version replay**: an older binary could reject or discard new event
  metadata. Mitigation: deploy descriptor-aware readers first, keep writes
  disabled until the fleet inventory is complete, and narrow rollback to
  descriptor-aware versions after activation.
- **Partial migration**: strict reads could strand historical content.
  Mitigation: inventory first, make migration idempotent, gate activation on a
  complete report, and leave the prior deployment active on failure.
- **Blob metadata lies or changes**: an external provider could report metadata
  inconsistent with delivered bytes. Mitigation: verify delivered length and
  digest as defense in depth and classify mismatch as integrity failure.
- **Stale storage reference**: provider configuration could stop resolving a
  persisted object identity. Mitigation: version the storage reference contract,
  require backend migration before configuration removal, and fail closed when
  no configured resolver supports it.
- **Unbounded recovery work**: replay or migration could scan unlimited streams.
  Mitigation: deterministic pages, explicit item/byte budgets, resumable durable
  cursors, and no background fan-out outside the bounded worker model.

### DST Compliance

- Descriptor collections and migration inventories use `BTreeMap`/`BTreeSet`
  and deterministic key ordering.
- Descriptor values use committed sequence numbers and content facts, never
  wall-clock time, random UUIDs, environment variables, or ambient filesystem
  access.
- Replay is a pure fold over ordered events. Query projections are caches and do
  not change the authoritative result.
- Trigger propagation uses the existing single-threaded actor/reaction path;
  no spawned work or nondeterministic race decides descriptor ownership.
- Blob stat/read operations remain behind injected storage traits with simulated
  implementations and fault schedules.

## Non-Goals

- Moving TemperFS app logic into the kernel.
- Making application `SizeBytes` or equivalent properties optional or removing
  their IOA invariants.
- Replacing content-addressed blob storage, deduplication, or the verified
  `StreamUpdated` transition.
- Introducing direct-to-provider upload sessions or changing `$value` response
  semantics.
- Generalizing FileVersion lifecycle or naming into every stream-capable app.
- Weakening Cedar, artifact grants, or per-invocation stream budgets.

## Alternatives Considered

1. **Add `SizeBytes` and `size_bytes` aliases** — Rejected because kernel safety
   would still depend on application vocabulary and every new schema would need
   another compatibility spelling.
2. **Resolve likely properties through generated manifest names** — Rejected
   because the manifest can translate a known semantic property but cannot
   prove that a property has length or ownership semantics merely from its name
   and scalar type.
3. **Add CSDL length and owner annotations** — Better than aliases and suitable
   for application projections, but still makes mutable domain state the sole
   authority for a pre-I/O security boundary. It also cannot by itself prove
   that the declared blob has that length. The descriptor may be exposed through
   CSDL annotations without changing its authority. This ADR does use closed
   CSDL annotations for mutability and typed relationships, where schema
   semantics rather than byte facts are authoritative.
4. **Fetch the blob and use `bytes.len()`** — Rejected because it enforces the
   budget only after the prohibited I/O and allocation have occurred.
5. **Trust object-store `Content-Length` on every open** — Rejected as the sole
   authority because it loses entity sequence and version ownership, makes
   correctness depend on provider availability, and cannot represent a
   committed-but-not-yet-visible object cleanly. Blob stat remains a validation
   and migration input.
6. **Keep a separate best-effort descriptor table** — Rejected because an event
   commit followed by a failed metadata write creates an ambiguous crash window.
   Derived tables may cache committed descriptors, but the journal envelope is
   authoritative.
7. **Trust WASM adapter callback metadata** — Rejected because a guest-selected
   hash, length, URL, or successful HTTP response does not attest which bytes the
   configured platform blob store can later return. Typed commits require an
   opaque host receipt.

## Rollback Policy

Before descriptor writes begin, rollback may return to the prior deployed
binary because journals contain only historical envelope versions. During the
reader-first deployment, strict admission and descriptor writes remain disabled
until every active and rollback binary is descriptor-aware.

After any descriptor-bearing event is committed, including a current-File
backfill before strict activation, rollback must use a descriptor-aware binary
and preserve and continue replaying descriptor metadata. The service may disable
typed stream opens and revert traffic to the last descriptor-aware deployment,
but it must not re-enable field-name inference, delete descriptors, or publish
content whose descriptor cannot be validated. Migration records and event
metadata are append-only; recovery moves forward with a corrected
descriptor-aware binary.
