# ADR-0027: OS App Catalog — Agent-Installable Pre-Built Apps

- Status: Accepted
- Date: 2026-03-10
- Deciders: Temper core maintainers
- Related:
  - ADR-0006: Spec-Aware Agent Interface
  - `.vision/AGENT_OS.md` (agent-first platform vision)
  - `crates/temper-platform/src/bootstrap.rs` (system tenant bootstrap pattern)
  - `os-apps/project-management/` (first OS app)

## Context

OS apps (like `os-apps/project-management/`) are pre-built spec bundles that ship with Temper. They exist as raw spec files but are not wired into the shipping product — no code loads them, agents cannot discover them, and developers do not know they exist.

The gap between "specs ship with the binary" and "agents can use them" is missing. Agents need a way to discover available apps and install them into a tenant, and developers need a CLI flag to pre-load apps at startup.

## Decision

### Sub-Decision 1: Embedded Catalog via `include_str!()`

Specs are embedded in the binary at compile time using `include_str!()`, following the same pattern as system tenant specs in `bootstrap.rs`. This ensures OS apps work on deployed binaries without path resolution issues.

A static `OS_APP_CATALOG` array holds metadata for each app (name, description, entity types, version). `get_os_app()` returns the full bundle (specs, CSDL, Cedar policies).

**Why this approach**: Mirrors the proven system bootstrap pattern. No filesystem dependency at runtime.

### Sub-Decision 2: HTTP Endpoints for Agent Discovery

Two new endpoints under `/api`:
- `GET /api/os-apps` — returns available apps with metadata
- `POST /api/os-apps/:name/install` with `{ "tenant": "..." }` — installs into a tenant

**Why this approach**: Agents interact via HTTP through the sandbox dispatch layer. RESTful endpoints fit the existing pattern.

### Sub-Decision 3: Agent Dispatch Methods

Two new methods in `temper-sandbox/src/dispatch.rs`:
- `list_apps()` — calls `GET /api/os-apps`
- `install_app(tenant, app_name)` — calls `POST /api/os-apps/:name/install`

**Why this approach**: Follows the existing dispatch pattern where every agent method maps to an HTTP call.

### Sub-Decision 4: CLI Flag `--os-app`

`temper serve --os-app project-management` installs the named OS app into the `default` tenant during Phase 8 (after `bootstrap_tenants`).

**Why this approach**: Matches existing `--app` pattern for developer convenience. Loads into `default` tenant to match existing default behavior.

### Sub-Decision 5: Per-Tenant Scoping

Install is always per-tenant: `install_app("my-project", "project-management")` scopes to one tenant. This reuses `bootstrap_tenant_specs()` which handles verification, CSDL registration, and SpecRegistry updates.

**Why this approach**: Consistent with the multi-tenant architecture. Each tenant sees only apps explicitly installed for it.

## Rollout Plan

1. **Phase 0 (This PR)** — Catalog types, embedded specs, HTTP endpoints, dispatch methods, CLI flag, tests.
2. **Phase 1 (Follow-up)** — Additional OS apps (CRM, helpdesk, etc.) added to catalog.
3. **Phase 2** — Observe UI shows installable apps in the developer dashboard.

## Consequences

### Positive
- Agents can discover and install pre-built apps on demand
- Developers get `--os-app` convenience flag for quick starts
- Scales to N apps — just add entries to the catalog
- Verification cascade runs at install time (same guarantees as system specs)

### Negative
- New API surface to maintain
- Binary size grows with each embedded OS app

### Risks
- If an OS app's specs fail verification, install returns an error (not a panic — unlike system bootstrap)

## Non-Goals

- Auto-installing OS apps on tenant creation (explicit install only)
- App marketplace or external app loading (embedded only for now)
- OS app versioning/upgrades (future work)

## Alternatives Considered

1. **Auto-bootstrap all OS apps** — Rejected: no opt-out, bloats every tenant with unused entities.
2. **CLI-only flag** — Rejected: agents cannot discover available apps.
3. **Agent-only catalog** — Rejected: no pre-load convenience for developers starting from CLI.

## Rollback Policy

Remove the `os_apps` module and related routes. No data migration needed — installed specs are just regular tenant specs.
