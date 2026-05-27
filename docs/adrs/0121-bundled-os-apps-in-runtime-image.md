# ADR-0121: Bundled OS Apps In Runtime Image

- Status: Accepted
- Date: 2026-05-26
- Deciders: Temper core maintainers
- Related:
  - ADR-0027: OS App Catalog
  - ADR-0120: Directed Evolution Control Plane
  - `Dockerfile`
  - `railway.toml`
  - `crates/temper-platform/src/os_apps/app_catalog.rs`

## Context

Temper production images currently copy only the compiled `temper` binary into the
runtime stage. That lets the server boot, but deployed processes cannot discover
repository-bundled OS apps because `os-apps/` is absent from the image. The
runtime log then reports:

```text
No os-apps directory found. Set TEMPER_OS_APPS_DIR for dev/local apps.
```

That was tolerable while deployed apps were recovered from durable state or
provided by another service, but it blocks new bundled OS apps such as Directed
Evolution from being installed or reconciled after the code is merged.

## Decision

The Temper runtime image will include the repository `os-apps/` directory under
`/opt/temper-os-apps` and set `TEMPER_OS_APPS_DIR=/opt/temper-os-apps`.

This keeps app discovery explicit and uses the existing first-priority app
catalog path. The server remains responsible for deciding which apps to install
per tenant; the image only makes the app bundles available.

The production Railway start command will explicitly pass
`--app directed-evolution`. This installs or reconciles the bundled Directed
Evolution app for the `default` tenant on boot using the normal governed app
installer, rather than requiring an ad-hoc post-deploy mutation endpoint.

## Rollout Plan

1. **Phase 0 (Immediate)** - Copy `os-apps/` into the runtime image and set
   `TEMPER_OS_APPS_DIR`.
2. **Phase 1 (Production deploy)** - Redeploy the Railway `temper-server`
   service with `--app directed-evolution` and verify the Directed Evolution
   entity sets are installed or recovered from the bundled source.
3. **Phase 2 (Follow-up)** - If image size becomes a problem, split runtime
   app packaging into a curated bundle manifest rather than copying every app.

## Readiness Gates

- Production logs show app catalog discovery from `/opt/temper-os-apps`.
- Directed Evolution app sources are available to the deployed server.
- `GET /tdata/Directions` for the production tenant no longer returns
  `EntitySetNotFound`.
- Existing tenant/app recovery still boots without requiring the working
  directory to contain source files.

## Consequences

### Positive

- New repository-bundled OS apps are deployable without a second artifact store.
- Runtime behavior matches local development more closely.
- Directed Evolution is installed into the production `default` tenant during
  the normal boot sequence after merge.

### Negative

- Runtime images are larger because all bundled OS app sources and WASM
  artifacts are copied.
- The production boot surface now includes Directed Evolution by default for the
  `default` tenant.

### Risks

- Copying every app may include source-only app material that is not needed at
  runtime. The mitigation is to keep app installation governed and add a
  manifest-based curated copy if size or exposure becomes an operational issue.

### DST Compliance

This changes only container packaging and startup configuration. No
simulation-visible runtime code changes.

## Non-Goals

- Auto-installing all OS apps for all tenants.
- Installing Directed Evolution into every tenant.
- Changing app discovery precedence.
- Replacing Genesis-published app bundles.

## Alternatives Considered

1. **Install Directed Evolution manually from a local path after deploy** -
   rejected because it would not survive normal production redeploys.
2. **Add a Directed Evolution-specific copy path** - rejected because the same
   deployment gap applies to future bundled OS apps.
3. **Publish app bundles to Genesis first** - useful long term, but it does not
   solve the existing Temper runtime image failing to include its own bundled
   apps.

## Rollback Policy

Remove the `TEMPER_OS_APPS_DIR` environment variable and the `COPY
--from=builder /app/os-apps /opt/temper-os-apps` line from the Dockerfile, drop
the `--app directed-evolution` start-command argument, then redeploy. Existing
persisted app state remains in the platform store.
