# ADR-0160: Genesis install supply-chain hardening

- Status: Accepted
- Date: 2026-07-12
- Deciders: Temper core maintainers
- Related:
  - `crates/temper-platform/src/genesis_install.rs` (registry install path)
  - `crates/temper-platform/src/tenant_api.rs` (`/api/genesis/apps/install`)
  - ARN-210 (security finding)

> This is Fable's competing entry for ARN-210; Grok's entry is PR #356. The two
> are compared head-to-head by the arena judge.

## Context

The Genesis install path fetches app bundles from a remote registry and writes
them under a local cache. Several boundary checks are missing (ARN-210):

- **Remote-controlled directory name.** A bundle's per-app `app.name` is joined
  straight onto the cache root and then `remove_dir_all`'d and written
  (`genesis_install.rs`). Individual *file* paths are validated
  (`safe_bundle_relative_path`), but the app *directory* name is not — a
  malicious or compromised registry returning `app.name = "../../etc"` (or an
  absolute path) escapes the cache root and drives an arbitrary filesystem
  delete + write.
- **SSRF + unbounded fetch.** Registry/bundle fetches use a default
  `reqwest::Client` with no timeout, following redirects, against any `http(s)`
  URL with no allowlist or private/link-local denial — so the install can scan
  internal networks and download unbounded data.

## Decision

### Sub-Decision 1: Validate every remote-controlled path component

`bundle_app_dir(cache_root, app_name)` requires `app_name` to be a single safe
path component — non-empty, not `..`, no separators, not absolute, not a
reserved component — before joining it under the cache root. It is used at every
site that materialized a per-app directory from a registry-supplied name, so a
traversal or absolute name is rejected before any `remove_dir_all`/write. This
mirrors the existing `safe_bundle_relative_path` posture for file paths.

### Sub-Decision 2: SSRF-safe, bounded registry fetches

Registry and bundle fetches go through one hardened client: connect/total
deadlines and redirects disabled, and the target host is rejected if it resolves
to a non-public address — loopback, private (RFC1918), link-local, CGNAT
(100.64.0.0/10), unspecified / `0.0.0.0/8`, broadcast, documentation, multicast,
IPv6 unique-local / link-local, and IPv4-mapped / IPv4-compatible IPv6 forms of
any of these. For DNS names the resolved address is *pinned* into the client
(`resolve`), so a rebinding second lookup can't swing the connection onto an
internal host after the check. The bundle response body is read under a byte
budget before decoding. The git-clone fallback host is checked with the same
classifier before egress.

## Consequences

### Positive
- A malicious registry can no longer escape the cache root to delete/write
  arbitrary files, scan the internal network, or force an unbounded download.

### DST Compliance
- `temper-platform` is not a simulation-visible crate (not temper-runtime /
  temper-jit / temper-server); the changes are pure validation + a bounded HTTP
  client, no wall clock in logic, no threads, no `HashMap`.

## Non-Goals / Follow-ups (scoped, documented per the arena's best-effort ask)

The disclosed remote **exploitation** surface (path escape, SSRF, unbounded
fetch) is closed here. Three further remediation items are larger / cross-cutting
and are recorded as follow-ups:
1. **Authenticated install authorization.** The platform `/api` surface has no
   per-request caller identity today, so requiring a Cedar tenant-owner /
   platform-admin capability on the install endpoint needs a platform identity
   model added first. Tracked as a follow-up (companion to the access-control
   work).
2. **Signed bundle digest verification** (verify a pinned signing identity /
   signed digest) needs registry signing-key infrastructure.
3. **Collision-resistant cache keys** — `sanitize_registry_id_component` is
   lossy; a collision-resistant scheme is a broader cache-key redesign.
4. **Pin git-fallback resolution.** The HTTP client pins its checked address, but
   the git-clone fallback (off by default) re-resolves at connect, leaving a
   narrow DNS-rebinding window. Closing it means resolving once and handing git a
   pinned address/URL — deferred because the fallback is admin/debug-only.
5. **Opt-in trusted-host allowlist for self-hosted registries.** The guard blocks
   all loopback/private hosts unconditionally. No in-repo flow installs from such
   a host today (production uses a public registry), so nothing that currently
   works is broken; a self-hosted or CI-local Genesis on a private address would
   need a documented, explicit opt-in env to be reachable.

## Alternatives Considered

1. **Reject only `..` textually.** Rejected: misses absolute paths, reserved
   components, and separators; component-based validation via `Path::components`
   is exhaustive.
