# ADR-0157: Genesis install security boundary (ARN-210)

- Status: Accepted
- Date: 2026-07-11
- Deciders: Temper core maintainers
- Related:
  - ARN-210: Genesis installer unauthenticated SSRF / filesystem-write / supply-chain
  - `crates/temper-platform/src/genesis_install.rs`
  - `crates/temper-platform/src/genesis_install_security.rs`
  - `crates/temper-platform/src/tenant_api.rs`

## Context

`POST /api/genesis/apps/install` accepted unauthenticated callers, any
`http(s)` registry URL, unbounded bundle bodies, and joined remote `app.name`
under the cache root without package-id validation. That combination is an
SSRF + destructive write + supply-chain boundary failure.

## Decision

### Sub-Decision 1: Authenticated install

Client-supplied `X-Temper-Principal-Kind` is **not** trusted for install.
Install mutations require a **verified** credential:

1. `Authorization: Bearer <TEMPER_PLATFORM_ADMIN_BEARER>` (preferred), or
2. `Authorization: Bearer <TEMPER_API_KEY>` when that platform key is set.

A dev-only escape hatch (`TEMPER_GENESIS_INSTALL_DEV_ADMIN=1` plus an admin
principal header) exists for local tests and is fail-closed when unset.

### Sub-Decision 2: Registry URL policy (SSRF)

- Prefer `https://`. Plain `http://` is limited to loopback when
  `TEMPER_GENESIS_ALLOW_HTTP_LOOPBACK` is set.
- Optional host allowlist via `TEMPER_GENESIS_REGISTRY_ALLOWLIST`.
- Reject userinfo, blocked hostnames (metadata/internal), and literal
  private/link-local/loopback/CGNAT IPs in the URL host.
- Registry HTTP clients disable redirects and apply connect/total timeouts.

### Sub-Decision 3: Path and package identity

- Validate `app.name` as a relative package id (no separators, no `..`).
- Join under cache only via `join_under_cache`.
- Stage bundle materialization in `*.staging`, then rename into the cache root.

### Sub-Decision 4: Budgets and integrity

- Bound bundle response bytes, apps per bundle, files per app, and file size.
- When the root app is present in the bundle, require `version_hash` match.
- Use collision-resistant cache keys (readable prefix + content hash suffix).

## Consequences

### Positive

- Unauthenticated install is denied.
- Default policy blocks common SSRF targets and path escape via package name.
- Unbounded bundle DoS is budgeted.

### Negative

- Local http registries need an explicit env opt-in.
- Operators using non-allowlisted hosts must configure the allowlist when set.

### Risks

- DNS rebinding to a public hostname that later resolves private is not fully
  closed without post-resolve pinning; follow-up may add connect-time IP checks.

## Non-Goals

- Full signed-bundle crypto verification (planned follow-up with Genesis
  registry signing identity).
- Changing the App.Install Cedar action model.

## Alternatives Considered

1. **Disable HTTP install entirely** — too restrictive for real Genesis ops.
2. **Per-tenant allowlist only** — still needed, but base private-IP deny is
   the fail-closed default.
