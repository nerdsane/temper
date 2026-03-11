# ADR-0027: TemperFS — A Governed File System on Temper Primitives

- Status: Accepted
- Date: 2026-03-10
- Deciders: Temper core maintainers
- Related:
  - ADR-0019: Agentic Filesystem Navigation (entity graph as navigable paths)
  - ADR-0026: Background Agent Capabilities (sub-agents, agent types)
  - `.vision/AGENT_OS.md` (Temper as agent operating layer)
  - `crates/temper-server/src/odata/` (HTTP surface)
  - `crates/temper-spec/` (IOA + CSDL)
  - `crates/temper-wasm/` (WASM integration runtime)

## Context

Temper positions itself as an operating layer for autonomous agents. An OS needs a filesystem. Today, agents operating through Temper have no standard way to:

1. **Store files and artifacts** — Agent-produced IOA specs, configurations, and documents have no governed storage
2. **Persist sandbox artifacts** — Coding agents working in sandboxes (E2B, Modal, local) have no way to "fsync" their work into Temper's governed layer
3. **Share files between agents** — No standard path-based namespace for agent-produced artifacts
4. **Audit file access** — File operations bypass Temper's event sourcing and Cedar authorization

### Research: How Others Solve This

**AgentFS (Turso)** uses SQLite as the single abstraction — dentry table (paths) + inode table (content) + toolcall audit log. FUSE is optional (added later for bash/grep compatibility). A copy-on-write overlay separates a read-only base layer from a writable delta layer. Large blobs are an unresolved problem — proposed answer is hybrid: small content inline in SQLite, large content via S3 pointer.

**The universal pattern across AgentFS, JuiceFS, GCSFuse, and every production system**: separate namespace/metadata from content/blobs. Metadata must be fast, transactional, queryable (Redis, SQLite, relational DB). Blobs must be cheap and scalable (S3, GCS). Systems that conflate them underperform.

**Sandbox persistence (Modal, E2B, Fly.io)** all use layered storage: immutable base + mutable overlay + snapshot-as-diff. Modal's insight: snapshots are images, images are mounted lazily — same infrastructure handles cold starts and restores.

**FUSE tradeoffs**: Gives LLM agents bash/grep compatibility (LLMs are pretrained on filesystem interactions, not custom APIs). Costs 1.5-3x overhead for metadata-heavy workloads. Object-storage FUSE has deep POSIX impedance mismatch (no atomic rename, no byte-range writes). AgentFS solved macOS by running a localhost NFS server instead of FUSE.

### Why Temper Is Uniquely Suited

Temper already provides what other systems build from scratch:
- **State machines** (IOA specs) → file lifecycle governance
- **Event sourcing** → complete audit trail of every file operation
- **Cedar authorization** → fine-grained access control per file
- **OData API** → rich query surface ($filter, $expand, $orderby)
- **Multi-tenancy** → per-agent/per-tenant file scoping
- **Verification cascade** → formally verify file system invariants
- **Navigation properties** → directory-file relationships via FK constraints
- **WASM integrations** → blob storage adapters with hot-reloadable logic

The gap is: nobody has assembled these primitives into a coherent filesystem abstraction.

## Decision

### Sub-Decision 1: TemperFS as a Temper App (IOA Specs + CSDL)

TemperFS is implemented as a standard Temper application — IOA specs define entity behavior, CSDL defines the OData schema. Two small framework enhancements support binary streaming:

1. **OData `$value` endpoint** — routes binary content to/from WASM blob adapters
2. **Streaming host functions** — bytes never enter WASM memory; WASM handles auth/orchestration, the host handles the data plane

**Four entities:**

**`Workspace`** — isolation and quota management
```
States: Active → Frozen → Archived
State vars:
  - quota_limit (counter): max bytes allowed
  - used_bytes (counter): current usage
  - file_count (counter): total files

Key actions:
  - Create: initialize workspace with quota
  - UpdateQuota: adjust quota limit
  - IncrementUsage: add bytes (guard: used_bytes + size_bytes <= quota_limit)
  - DecrementUsage: remove bytes
  - Freeze/Thaw: suspend/resume workspace
  - Archive: soft-delete

Invariant: used_bytes <= quota_limit (in Active state)
```

**`Directory`** — namespace nodes
```
States: Active → Archived
State vars:
  - item_count (counter): number of direct children

Key actions:
  - Create: params: name, path, parent_id, workspace_id
  - AddChild/RemoveChild: manage child count
  - Rename: changes name and path
  - Archive: soft-delete (guard: item_count == 0)

Invariant: item_count >= 0
```

**`File`** — content nodes with governed lifecycle
```
States: Created → Ready → Locked → Archived
State vars:
  - version_count (counter): monotonically increasing
  - size_bytes (counter): content size
  - has_content (bool): whether content has been uploaded
  - content_hash (string): content-addressable hash (e.g., sha256:...)
  - mime_type (string): MIME type

Key actions:
  - Create: params: name, path, directory_id, workspace_id, mime_type
  - StreamUpdated (from: [Created, Ready], to: Ready):
    Fired by $value PUT handler after WASM blob upload succeeds.
    Params: content_hash, size_bytes, mime_type
    NOT callable from Locked — transition table rejects → 409 Conflict
  - Lock (from: Ready, to: Locked)
  - Unlock (from: Locked, to: Ready)
  - Archive (from: [Ready, Locked], to: Archived)

Invariants:
  - Ready files have content: status == Ready => has_content == true
  - version_count monotonically increases
```

**`FileVersion`** — immutable version snapshots
```
States: Current → Superseded
State vars:
  - file_id (string): FK to File
  - version_number (counter): version at time of snapshot
  - content_hash (string): hash of this version's content
  - size_bytes (counter): size of this version
  - created_by (string): agent identity

Key actions:
  - Create: initialize with version metadata
  - Supersede: Current→Superseded (triggered by reaction when new version uploaded)
```

**Why four entities**: Workspace provides isolation and quotas — essential for multi-agent environments. Directory is separate because it has different lifecycle and invariants than File. FileVersion is separate because versions are immutable — different state machine than mutable files.

**Cross-entity reactions:**
- `File.StreamUpdated` → creates `FileVersion` (via ReactionDispatcher)
- `File.StreamUpdated` → supersedes previous `FileVersion`
- `File.StreamUpdated` → increments `Workspace.used_bytes`

### Sub-Decision 2: Uniform Blob Storage — All Content to External Store

All file content goes to blob storage (S3, R2, GCS — configurable per tenant). No inline tier.

The `content_hash` field stores a content-addressable key: `sha256:<hash>`. This gives automatic deduplication — two agents storing the same file create one blob.

**Why no inline tier**: The two-tier approach (inline ≤64KB + external >64KB) adds complexity without proportional benefit. Event journal bloat from inline content is worse than a blob store round-trip. Uniform storage simplifies the blob_adapter WASM module and eliminates edge cases around the threshold boundary.

**Why content-addressable**: Deduplication is free. Two agents storing the same file create one blob. Version history shares unchanged content. This is the Git object model applied to Temper.

### Sub-Decision 3: `$value` as Framework, WASM as Blob Handler

The OData `$value` endpoint is a **framework enhancement** — it routes binary content through WASM blob adapters. The flow:

**Upload (`PUT /Files('f-1')/$value`):**
1. Server receives raw bytes
2. Registers bytes in StreamRegistry (host-side, never enters WASM memory)
3. Invokes WASM blob_adapter with stream ID + metadata
4. WASM computes hash (`host_hash_stream`), checks cache (`host_cache_contains`), uploads to R2 (`host_http_call_stream`)
5. WASM returns action name + params (e.g., `StreamUpdated` with `content_hash`)
6. Server dispatches whatever action WASM returned (generic — server doesn't know about StreamUpdated)
7. Reactions fire: FileVersion.Create, Workspace.IncrementUsage

**Download (`GET /Files('f-1')/$value`):**
1. Server gets entity state (includes `content_hash`, `mime_type`)
2. Invokes WASM blob_adapter with entity state
3. WASM checks cache, downloads from R2 if needed, returns bytes to StreamRegistry
4. Server reads bytes from StreamRegistry, returns as raw HTTP response

**Critical properties:**
- The `$value` handler is **100% generic** — works for ANY entity type with `HasStream=true`
- Bytes **never enter WASM memory** — WASM references them by stream ID via host functions
- Hash algorithm, cache strategy, dedup logic, action name + params are ALL in the WASM blob_adapter and are **hot-reloadable**
- Event journal only sees metadata (content_hash, size_bytes) — no binary content

### Sub-Decision 4: Streaming Host Functions — No WASM Memory Limit

Bytes flow through StreamRegistry on the host side. WASM modules reference data by stream ID via five new host functions:

1. **`host_http_call_stream`** — HTTP with stream-based body/response. Host reads request body from StreamRegistry, makes HTTP call, stores response in StreamRegistry. WASM never touches raw bytes.

2. **`host_hash_stream`** — Compute hash of stream bytes. Hash algorithm chosen by WASM (hot-reloadable), computed by host. Returns hex-encoded hash to WASM.

3. **`host_cache_contains`** — Check if bytes are cached (WASM controls cache strategy).

4. **`host_cache_to_stream`** — Copy cached bytes to a stream (for subsequent operations).

5. **`host_cache_from_stream`** — Cache bytes from a stream (WASM decides what/when to cache).

**Why streaming, not in-memory**: WASM modules have a memory limit (default 16MB). Files can be gigabytes. By keeping bytes in the host's StreamRegistry and giving WASM only stream IDs, there is no file size limit. A 1GB file works the same as a 1KB file.

**StreamRegistry design**: A dumb byte store with two categories:
- **Streams**: temporary, per-invocation (registered before WASM, consumed after)
- **Cache**: persisted across invocations, LRU safety eviction to prevent OOM

StreamRegistry makes NO caching decisions — WASM controls all caching strategy via `host_cache_*` functions.

### Sub-Decision 5: Cache Control via WASM Host Functions

Caching strategy lives in the WASM blob_adapter (hot-reloadable), not in the server framework:

- WASM decides whether to check cache before downloading
- WASM decides whether to populate cache after downloading
- WASM decides cache key format (currently `sha256:hash`, could change)
- WASM decides dedup strategy (currently cache-based, could add R2 HEAD check)

The StreamRegistry provides only a **safety budget** (default 256MB) with LRU eviction when the budget is exceeded. This is OOM prevention, not caching policy.

**Why in WASM**: Change eviction policy, skip caching for large files, add pre-warming — all by updating `blob_adapter.wasm`. No Rust recompile, no server restart. Caching strategy is a product decision that changes frequently — it belongs in the hot-reloadable layer.

### Sub-Decision 6: OData API is the Primary Interface — FUSE Maps 1:1

FUSE is deferred. The OData API is the primary interface. But the API is designed for single-call FUSE compatibility:

```
PUT  /Files('f-1')/$value       → write(fd, buf, size)    [single HTTP call]
GET  /Files('f-1')/$value       → read(fd, buf, size)     [single HTTP call]
POST /Files {name: "f.txt"...}  → open(path, O_CREAT)     [single HTTP call]
GET  /Files('f-1')              → stat(path)               [single HTTP call]
POST /Files('f-1')/NS.Lock      → flock(fd, LOCK_EX)      [single HTTP call]
POST /Files('f-1')/NS.Unlock    → flock(fd, LOCK_UN)      [single HTTP call]
```

Each POSIX operation maps to exactly one OData call — no multi-step sequences.

### Sub-Decision 7: Cedar Authorization for File Access

File access is governed by Cedar policies, not POSIX permission bits:

```cedar
// Agents can read/write files in their own workspace
permit(principal is Agent,
    action in [Action::"StreamUpdated", Action::"Create", Action::"Lock", Action::"Unlock"],
    resource is File)
when { resource.workspaceId == principal.workspaceId };

// Locked files reject StreamUpdated (defense in depth)
forbid(principal, action == Action::"StreamUpdated", resource is File)
when { resource.status == "Locked" };

// Archive requires workspace admin
permit(principal is Agent, action == Action::"Archive", resource is File)
when { principal in Group::"WorkspaceAdmins" && resource.workspaceId == principal.workspaceId };
```

**Why Cedar, not POSIX bits**: POSIX permissions are too coarse for multi-agent environments. Cedar gives attribute-based, policy-driven access control that can express "agent A can read files in agent B's namespace if they're in the same project" — something POSIX can't.

### Sub-Decision 8: Sandbox fsync — Diff-Based Snapshot to TemperFS

When a coding agent finishes work in a sandbox (E2B, Modal, local container), it "fsyncs" the sandbox state to TemperFS:

1. **Diff**: Compare sandbox filesystem against the base image to find changed/new files
2. **Upload**: For each changed file, create or update a File entity in TemperFS
3. **Snapshot**: Create a Directory snapshot (a new Directory entity representing the point-in-time state)

The fsync operation is an agent action, not a framework primitive. An agent runs the diff, then calls the OData API to create/update File entities.

### Sub-Decision 9: Verification Use Case — IOA Spec Storage

The validation use case is **IOA spec file storage** — agents upload, download, version, and lock spec files through TemperFS.

**Why spec storage**:
- It's a real, immediate need (spec files are the core artifact in Temper)
- It exercises the full lifecycle (create file, upload content, version, lock, read back)
- It validates binary streaming ($value endpoints work correctly)
- Files are small enough for quick tests but the architecture handles any size

**The test scenario:**
1. Create a Workspace with quota
2. Create a Directory `/specs`
3. Create a File `Order.ioa.toml`
4. `PUT /Files('F')/$value` → upload raw TOML bytes → WASM blob upload → StreamUpdated → 204
5. Verify: FileVersion created via reaction (version_number=1, status=Current)
6. `GET /Files('F')/$value` → download bytes → bytes match original
7. Upload updated spec → version_count=2, old FileVersion superseded
8. Lock file → `PUT $value` returns 409 Conflict
9. Unlock file → verify Workspace.used_bytes tracks total
10. Upload identical content → CAS dedup (blob already cached, skip upload)

## Rollout Plan

1. **Phase 1 (Framework)** — OData `$value` path parsing, `HasStream` CSDL attribute, `ODataStreamResponse`, StreamRegistry, streaming host functions, `$value` GET/PUT handlers, direct WASM invocation
2. **Phase 2 (App)** — IOA specs (Workspace, Directory, File, FileVersion), CSDL schema, Cedar policies, reaction rules, WASM blob_adapter
3. **Phase 3 (Integration)** — End-to-end spec storage test, SimWasmHost extensions for binary responses

## Readiness Gates

- All four IOA specs pass L0-L3 verification
- Spec storage integration test passes end-to-end
- Blob upload/download round-trips correctly through WASM streaming
- Cedar policies correctly scope file access per workspace
- CAS dedup eliminates redundant blob transfers
- Reactions correctly create FileVersions and update Workspace usage

## Consequences

### Positive
- Agents get a governed, auditable, multi-tenant filesystem
- Every file operation has a formal state machine, event trail, and authorization check
- Content-addressable storage gives free deduplication
- Validates that Temper primitives are expressive enough for OS-level infrastructure
- No file size limit — streaming host functions bypass WASM memory constraints
- Hot-reloadable blob handling — change hash algorithm, cache strategy, storage backend without recompilation
- FileVersion tracking via reactions — no client-side coordination needed
- Single-call FUSE compatibility — each operation maps to one HTTP call

### Negative
- No POSIX compatibility initially — agents must use OData API (mitigated by ADR-0019's navigate pattern)
- Two small framework enhancements needed ($value routing, streaming host functions) — but these are generic and benefit all HasStream entities
- External blob storage is an additional infrastructure dependency

### Risks
- **IOA expressiveness**: The file lifecycle may need state variables or guard patterns not yet supported. Mitigation: this is exactly the kind of pressure test that improves the spec language.
- **Performance**: Large directory listings via OData may be slow. Mitigation: $top/$skip pagination, index on `path` field.
- **Blob store reliability**: External blob store failures during upload. Mitigation: WASM blob_adapter returns error, $value handler returns 502. File remains in previous state (no partial transitions).

### DST Compliance
Framework changes (StreamRegistry, streaming host functions) use BTreeMap + VecDeque for deterministic behavior. SimWasmHost has canned binary responses for testing. No HashMap, no std::fs, no wall-clock time in any simulation path.

TemperFS app code (IOA specs, CSDL, Cedar, reactions) goes through the existing actor system which is already DST-compliant.

## Hot-Reloadability Analysis

| Component | Hot-reloadable? | Location |
|-----------|-----------------|----------|
| File/Directory/Workspace states, transitions, guards | Yes | `os-apps/temper-fs/specs/*.ioa.toml` |
| Authorization policies | Yes | `os-apps/temper-fs/policies/*.cedar` |
| Reaction rules (FileVersion, Workspace usage) | Yes | `os-apps/temper-fs/reactions/reactions.toml` |
| Blob storage auth (S3 Sig V4) | Yes | `os-apps/temper-fs/wasm/blob_adapter/` |
| Caching strategy (check, populate, dedup) | Yes | `os-apps/temper-fs/wasm/blob_adapter/` |
| Hash algorithm (SHA-256, BLAKE3, etc.) | Yes | `os-apps/temper-fs/wasm/blob_adapter/` via `host_hash_stream` |
| Action name + params (StreamUpdated, etc.) | Yes | `os-apps/temper-fs/wasm/blob_adapter/` returns action/params |
| URL construction (endpoint, bucket) | Yes | `os-apps/temper-fs/wasm/blob_adapter/` via integration_config |
| CSDL schema (HasStream, nav properties) | Yes | `os-apps/temper-fs/schema/model.csdl.xml` |
| `$value` endpoint routing | No | `crates/temper-server/` (generic framework) |
| StreamRegistry (dumb byte store) | No | `crates/temper-wasm/` (generic framework) |
| Host functions (stream, cache, hash) | No | `crates/temper-wasm/` (generic framework) |
| ODataStreamResponse | No | `crates/temper-server/` (generic framework) |

The Rust framework code is **generic** — it works for any entity type with `HasStream=true`, not just TemperFS. All filesystem-specific logic is in the app.

## Non-Goals

- **Replacing S3/GCS** — TemperFS manages metadata and lifecycle; blob stores handle content
- **Full POSIX semantics** — No hardlinks, no byte-range writes, no inotify, no mmap
- **Real-time sync** — No live collaboration (Google Docs style); use polling or evolution engine
- **Database storage** — TemperFS is for files (documents, conversations, artifacts), not structured data

## Alternatives Considered

1. **SQLite-backed filesystem (AgentFS clone)** — SQLite is elegant but bypasses Temper's entire governance layer. No verification, no Cedar, no event sourcing. Rejected because it undermines the core thesis: everything goes through the governed layer.

2. **FUSE-first approach** — Start with FUSE mount over OData. Rejected because FUSE adds kernel/OS complexity before we've validated the data model. The research confirms: native API first, FUSE later.

3. **Flat key-value store (no directories)** — Simpler but loses hierarchical navigation. Agents think in paths, not keys. ADR-0019 already established entity-graph-as-filesystem navigation. Rejected.

4. **Two-tier inline/external storage** — Store small files (≤64KB) inline in entity state, large files in blob store. Rejected: adds complexity (base64 encoding, threshold edge cases) without proportional benefit. Uniform blob storage is simpler and avoids event journal bloat.

5. **Presigned URLs (client downloads directly from blob store)** — Two-step flow: get presigned URL, then download. Rejected: WASM blob_adapter provides single-call semantics, server proxies efficiently via streaming, and caching is transparent.

6. **Dedicated storage crate (temper-fs)** — New Rust crate for file operations. Rejected because TemperFS should be a Temper app, not framework code. The two framework enhancements ($value routing, streaming host functions) are generic and benefit all apps.

## Rollback Policy

Framework enhancements ($value routing, StreamRegistry, streaming host functions) are additive — they don't modify existing behavior. Rollback is removing unused code.

TemperFS app (IOA specs, CSDL, Cedar, reactions, WASM blob_adapter) lives in `os-apps/temper-fs/` and can be unregistered from SpecRegistry without affecting other apps.
