# ADR-0180: Local-First Immutable App Bundles

- Status: Accepted
- Date: 2026-08-24
- Deciders: Temper core maintainers
- Supersedes: ADR-0043 only where it requires Genesis as the exclusive install source
- Related:
  - ADR-0043: Git-Based App Sources
  - ADR-0062: Delta OS App Reconcile and WASM Artifacts
  - ADR-0122: Genesis Pinned App Install
  - `crates/temper-platform/src/genesis_install.rs`
  - `crates/temper-platform/src/os_apps/`
  - `crates/temper-cli/src/`

## Context

Temper can already verify and run specs locally, persist them in embedded
libSQL, store blobs on the filesystem, expose Observe, and provide an MCP
bridge. Installing a complete app, however, normally starts from a pinned
Genesis ref. This makes publication infrastructure part of the first-run path
even when a developer only wants a private app on one machine.

Earlier local sources were mutable catalog directories, symlink farms, or
startup-time Git clones. ADR-0043 correctly removed those as installation
sources: a directory could change after installation, dependencies could be
found through ambient filesystem state, and restart provenance did not prove
which bytes had run. The missing capability is not another mutable source. It
is a local producer for the same immutable, bounded bundle consumed by the
installer after Genesis resolution.

The local distribution must also be self-contained. The current development
server assumes a source checkout and a separately installed Next.js tree for
Observe, while MCP clients normally launch an additional stdio bridge. Those
are useful development paths but are not a zero-configuration local product.

## Decision

### Sub-Decision 1: One canonical bundle contract

Temper defines `CanonicalBundleManifestV1`. A manifest identifies one root app
and its complete dependency closure. Each app entry contains its name, version,
resolved dependency identities, and sorted regular-file records. Each file
record contains its normalized app-relative path, byte length, and SHA-256 blob
digest.

The bundle digest is SHA-256 over a domain-separated, length-prefixed encoding
of the schema version, root identity, sorted app records, sorted dependency
edges, and sorted file records. It does not include source paths, filesystem
timestamps, directory iteration order, registry URLs, or Genesis Git hashes.
The manifest is JSON for inspection, but JSON serialization is not the digest
input.

Local workspace scanning and Genesis transport decoding are source adapters.
Both feed the same canonical builder, budget accounting, path validation,
dependency resolver, cache, and installation service. Genesis Git hashes
remain publication provenance; the SHA-256 bundle digest is the runtime content
pin.

**Why this approach**: Source-specific resolution remains at the edge while
every security and installation rule applies to the bytes that actually run.

### Sub-Decision 2: Manifest-and-blob cache

The cache lives beneath the selected Temper data directory:

```text
bundles/v1/manifests/sha256/<bundle-hex>.json
bundles/v1/blobs/sha256/<blob-hex>
bundles/v1/views/sha256/<bundle-hex>/<app-name>/...
```

Objects are written to same-filesystem staging paths, flushed, validated, and
published with atomic renames. Existing objects must byte-match their digest.
Materialized views contain only validated regular files and are disposable;
they can always be rebuilt from the manifest and blobs. Install and recovery
revalidate the manifest and every referenced blob before registering a view.
Neither operation falls back to a recorded workspace path.

The validator rejects empty or absolute paths, `.` and `..`, platform-prefix
components, `.git`, generated `target` trees, symlinks, special files,
duplicate app names or paths, files that change while read, and all existing
per-file, aggregate-byte, file-count, tree-depth, tree-entry, and app-count
budget violations.

Garbage collection is mark-and-sweep. Durable installed-app rows and in-flight
publication leases are roots. Referenced manifests retain all their blobs.
Corrupt objects are quarantined and surfaced as readiness and Observe failures;
they are never silently replaced from mutable source state.

**Why this approach**: Content-addressed blobs deduplicate dependency closures
and make crash recovery and integrity checks explicit without forcing the
installer to parse an archive format.

### Sub-Decision 3: Explicit local dependency lock

For a local workspace snapshot, `app.toml` declares dependency names and
`temper.lock.toml` records the explicit local path plus resolved bundle digest
for every member of the closure. Paths are resolved relative to the lock file
and are never searched for by name. Pinned Genesis refs remain supported by
the governed Genesis install path, which now converts the resolved closure to
the same canonical bundle contract before installation; the local workspace
adapter rejects mixed local/Genesis closures instead of resolving them through
ambient state.

`temper app lock --local NAME=PATH` updates the lock atomically. Normal local
install and development rebuild the closure and refresh resolved digests after
successful verification. `--locked` instead fails when the lock is absent,
incomplete, or stale. A dependency without an explicit locked local path is an
error in the local adapter; a pinned Genesis ref is an error with guidance to
use governed `App.Install`. Cycles, duplicate names, conflicting refs, and
closure budget exhaustion fail before cache or runtime mutation.

Only resolved names, dependency edges, and content digests enter the canonical
bundle. Local filesystem paths remain build provenance and never affect bundle
identity.

**Why this approach**: Multi-app local work remains possible without restoring
ambient lookup or making machine-specific paths part of the installed artifact,
while pinned publication dependencies retain Genesis governance.

### Sub-Decision 4: Source-neutral governed installation and provenance

A single internal `install_canonical_bundle` service accepts a validated cache
object, target tenant, and typed provenance. Genesis `App.Install` calls it
after resolving the pinned registry ref. The local bundle HTTP endpoint
authorizes `install_app_bundle` against the target tenant and bundle digest,
then calls the same service. The endpoint accepts bundle bytes, never a path for
the server to read.

Installed-app source kinds are `builtin`, `local_bundle`, and `genesis`.
Existing `local` rows are interpreted as `builtin`. Local records store the
bundle digest and closure ID as the immutable pin plus a display-only source
locator and lock digest. Genesis records retain registry URL, registry tenant,
app ref, Git version, and follow policy in addition to the canonical digest.

Restart restoration selects behavior by source kind:

- `builtin` reloads the matching embedded catalog bundle;
- `local_bundle` reconstructs its view only from cached manifest and blobs;
- `genesis` uses a valid cached bundle or rematerializes the pinned registry
  ref before entering the canonical installer.

**Why this approach**: Governance and runtime behavior are identical after
resolution, while provenance still answers where the content came from.

### Sub-Decision 5: Local CLI lifecycle

The supported local-first surface is:

```console
temper up
temper app install ./my-app
temper dev ./my-app
```

`temper up` is a foreground daemon. By default it binds `127.0.0.1:3000`, uses
embedded libSQL and filesystem blobs beneath the platform data directory,
restores installed bundles, serves Observe and MCP, and opens Observe unless
`--no-open` is passed. Binding a non-loopback address is not part of `up`; users
must use the explicit hosted/server configuration for remote ingress.

`temper app install` snapshots and uploads a canonical bundle to an existing
daemon. `temper dev` connects to that daemon or starts an embedded one, watches
the root and locked local dependencies, and builds an immutable revision after
changes settle. Verification covers the complete closure before installation
begins. A failed build reports the complete verification failure and leaves the
last good digest active. Installation then uses the existing governed,
durable-first app reconciliation boundary dependency-by-dependency; the
canonical root provenance is recorded only after the complete closure succeeds.

`temper serve`, pinned Genesis installation, helper-skill installation, and the
stdio MCP bridge remain supported. `serve --app NAME=DIR` becomes a compatibility
entry point for a one-shot immutable snapshot rather than a retained mutable
installation. `temper init` produces a valid app manifest, guide, IOA/CSDL
example, and empty v1 lock.

**Why this approach**: The short path teaches the production artifact model;
development convenience does not introduce a second mutable runtime model.

### Sub-Decision 6: Generated local credentials

On first startup, `temper up` generates a cryptographically random operator
credential and writes it atomically beneath the selected data directory with
owner-only permissions. Existing credentials with broader permissions are
rejected rather than silently used. Local CLI commands discover this file for
the default loopback endpoint. Remote or custom endpoints require explicit
credentials.

The bootstrap credential is a normal verified, tenant-scoped operator. Its
bootstrap policy permits the local administration operations required to
install bundles; application actions remain governed by their app policies.

Observe exchanges a one-time loopback bootstrap nonce for an HttpOnly,
SameSite=Strict session cookie. The durable operator token is not embedded in
static assets, URLs, browser storage, or logs.

**Why this approach**: A permission-restricted file works in headless and
cross-platform installations without making unauthenticated localhost a trust
boundary.

### Sub-Decision 7: Embedded local Observe, separate hosted build

Observe has two delivery targets sharing the same authenticated Observe HTTP
contracts:

- the existing Next.js server retains GitHub OAuth, middleware, and hosted
  proxy behavior;
- a dependency-free diagnostic shell is embedded in the Temper binary and
  exposes health, specs, entities, workflows, trajectories, agents, and WASM
  views through same-origin APIs using the local session cookie.

The Rust server serves the local shell beneath `/observe`. It is compiled into
the Rust binary; running a released binary never requires Node or a Temper
source checkout. The hosted React application remains the richer operational
UI and consumes the same server contracts.

**Why this approach**: Local startup becomes self-contained without removing
the working hosted authentication path.

### Sub-Decision 8: Stateless Streamable HTTP MCP

`temper up` exposes `POST /mcp` using MCP protocol revision `2026-07-28`.
Every request validates bearer authentication, the `Origin` header,
`MCP-Protocol-Version`, `Mcp-Method`, and where applicable `Mcp-Name`, including
header/body equality. Responses are JSON or request-scoped SSE. GET and DELETE
return 405; no transport session ID is created.

Each HTTP request receives a fresh sandbox and a one-turn OTS trajectory. Agent
identity comes from the verified credential; client metadata is observational
and cannot grant authority. Host-path operations are disabled over HTTP. The
existing stdio server retains its compatibility handshake and session-scoped
trajectory behavior.

**Why this approach**: This follows the current stateless MCP transport and
avoids adding a hidden session store solely for local access.

## Rollout Plan

1. **Canonical core** — Land the manifest, cache, source adapters, typed
   provenance, store migrations, shared installer, and recovery.
2. **Local lifecycle** — Land `up`, `app lock`, `app install`, `app cache gc`,
   `dev`, generated credentials, and updated scaffolding.
3. **Local surfaces** — Embed a dependency-free Observe diagnostic surface and
   add stateless HTTP MCP while retaining the richer hosted React Observe and
   stdio compatibility. Both Observe surfaces use the same authenticated server
   contracts.
4. **Proof and documentation** — Make local-only operation the default quick
   start, run the restart E2E, and verify hosted Genesis/Observe regressions in
   the deployed system.

## Readiness Gates

- A workspace installs without a Genesis account, Git server, Docker,
  PostgreSQL, or hosted service.
- Editing the source cannot change an installed app until a new verified
  digest is explicitly promoted.
- Restart restores the exact digest and source provenance without the original
  workspace.
- Local and Genesis sources pass the same canonical validation, budgets,
  dependency resolution, verification, and installation service.
- Observe and authenticated HTTP MCP work from a released local binary without
  Node or a separate bridge process.
- Hosted Genesis installs, hosted Observe authentication, and stdio MCP remain
  operational.

## Consequences

### Positive

- Local-only development becomes the shortest supported path.
- Installation provenance names exact content rather than a mutable directory.
- Genesis remains valuable for publication and collaboration without becoming
  a local runtime dependency.
- Cache integrity, dependency closure, recovery, and GC use one model for every
  source.

### Negative

- The compact embedded Observe diagnostic surface is intentionally less rich
  than the hosted React application and must track its server contracts.
- Local installation uploads and validates all uncached bundle bytes even when
  client and server run on the same machine.
- The manifest, cache, provenance migration, and HTTP MCP transport add
  permanent compatibility surfaces.

### Risks

- Interrupted cache publication could strand staging files. Startup and GC
  remove only unleased staging objects and never treat them as valid bundles.
- A compromised local operator token grants tenant administration. Strict file
  permissions, loopback binding, origin checks, and cookie separation reduce
  exposure.
- A source can change during snapshotting. Bounded reads verify metadata and
  content; any change aborts the revision.
- Divergent local and hosted Observe components could drift. Shared view/API
  packages and CI builds for both targets gate releases.

### DST Compliance

Cache, CLI, and platform source resolution are outside simulation-visible
execution. The HTTP route touches `temper-server`; it performs bounded request
handling and creates fresh request state without wall-clock or random behavior
inside simulated actors. Credential randomness and filesystem operations occur
only in CLI startup code and will carry narrow `// determinism-ok` annotations
where the guard requires them.

## Non-Goals

- Replacing Genesis publication, discovery, collaboration, or provenance.
- Adding Git hosting, Railway deployment logic, or automatic public ingress to
  the Temper kernel.
- Restoring symlink catalogs, runtime Git clones, or mutable installed paths.
- Weakening verification, Cedar authorization, resource budgets, or durable
  pins.
- Removing existing hosted or stdio access paths.

## Alternatives Considered

1. **Immutable digest directory** — Simpler, but duplicates dependency content
   and makes blob-level integrity and GC less useful.
2. **Canonical archive** — Portable, but adds archive parsing and extraction
   risks before the existing directory loader can run.
3. **Preview tenant for `temper dev`** — Strong isolation, but adds promotion
   ceremony to the default loop without improving immutable revision safety.
4. **OS-keychain credentials** — Better platform integration, but inconsistent
   in headless environments and requires platform-specific dependencies.
5. **One static Observe build everywhere** — Would require moving or removing
   the working hosted GitHub authentication boundary in the same change.
6. **Session-based HTTP MCP** — Conflicts with the current MCP transport and
   requires state that the protocol intentionally removed.

## Rollback Policy

Keep the canonical cache readable and retain provenance columns. Disable the
new CLI commands and HTTP routes, then continue restoring existing builtin and
Genesis records through the canonical installer. Never reinterpret a local
bundle record as a mutable workspace install; users may publish that digest to
Genesis or explicitly uninstall it.
