# ADR-0053: Datadog Service Decoupling — `temper` vs `openpaw`

- Status: **Superseded 2026-04-20**
- Date: 2026-04-18
- Deciders: Temper core maintainers
- Related:
  - ADR-0052: Instrumentation as policy
  - ADR-0054: Log standard
  - ADR-0055: Continuous profiling

## Supersession note (2026-04-20)

This ADR proposed emitting two services — `service:temper` for platform spans and `service:openpaw` for embodiment spans — from a single process, via a second OpenTelemetry `SdkTracerProvider`.

**That design was wrong.** OpenTelemetry's resource model pins `service.name` to the provider; emitting two from one process requires either explicit tracer-handle plumbing at every call site (loses `tracing::info_span!` / `#[instrument]` ergonomics) or span-name filter routing at the subscriber layer (policy-as-string — rename a span, routing silently breaks).

The correct model is **one `service.name` per process**, named after the deployment:

| Deployment | `service.name` | Temper presence |
|---|---|---|
| Railway openpaw server (today) | `openpaw` | Library linked in |
| Standalone Temper server (hypothetical) | `temper` | It IS the service |
| Tamago Mac app | `tamago` | Library linked in |
| Future embodiment | whatever | Library linked in |

Cross-deployment "all Temper activity" queries use span attributes (e.g. `span.name:dispatch.*`) or the existing `temper_*` metric prefix, not a service-level filter. Datadog's Service Catalog gains entries as embodiments ship — each deployment is one catalog row, which is what the catalog was designed for.

**What changed in code 2026-04-20:**
- Removed `openpaw_tracer()` / `install_openpaw_tracer()` / `OPENPAW_TRACER` static / second `SdkTracerProvider` from `crates/temper-observe/src/otel.rs`.
- `OtelGuard` no longer carries `openpaw_tracer_provider`.
- `embodiment` resource tag already removed in ADR-0053 Sub-Decision 3 rewrite (nerdsane/temper#153).

**What did not change:**
- `@runtime-id` resource tag stays (ADR-0055, profile↔trace stitching).
- `service.name = openpaw` continues to be set via the `DD_SERVICE` env or the single `SdkTracerProvider`'s service-name resource attribute — unchanged from production today.
- When a new deployment ships (Tamago, standalone Temper), it sets `DD_SERVICE` to its own name. No framework work required.

The original decision text below is preserved for the historical record. Do not treat it as current design.

## Context

Every span, metric, and log in production today carries `service:openpaw`. The openpaw-server binary contains two logically distinct tiers:

- **Temper (platform)**: dispatch, actor runtime, event store, WASM host, admission control, state-timeouts, Cedar, OTel pipeline. Implemented in `crates/temper-runtime`, `crates/temper-server`, `crates/temper-spec`.
- **OpenPaw (embodiment)**: Slack / Discord / HTTP triggers, app installer, dashboard, CLI. Implemented in `crates/openpaw`, `crates/paw-triggers`.

Conflating them in one `service` attribute creates three concrete problems:

1. **SLO mixing**. Latency SLOs on `service:openpaw` include both platform dispatch time (what Temper owns) and trigger routing overhead (what OpenPaw owns). Neither team's SLO is measured accurately.
2. **Service Catalog is useless for Temper**. When Temper ships inside Tamago (macOS menu bar app, in active development) and future embodiments, we will want one catalog entry for the Temper platform that tracks all deployments regardless of host. Today Temper has no catalog identity at all.
3. **Query friction**. `resource_name:Session.*` cannot distinguish "platform dispatching Session" from "OpenPaw HTTP wrapping a Session dispatch". Operators learn to live with ambiguity; they shouldn't have to.

The user is actively building a second embodiment (Tamago). This is the last reasonable moment to split the service identity before the problem spreads to multiple codebases.

## Decision

Emit two services from one process. Platform code gets `service:temper`; embodiment code gets `service:openpaw`. Cross-service traces are stitched by trace ID.

### Sub-Decision 1: Per-span service name via OTel resource overrides

OpenTelemetry allows a tracer provider to carry a default resource and individual spans to override attributes. We use that mechanism to attach `service.name` per span according to the code path that created it.

Concretely, `temper-server` gets a new module `crates/temper-server/src/otel.rs` that exposes two `Tracer` handles:

```rust
pub fn temper_tracer() -> &'static Tracer { /* service.name=temper */ }
pub fn openpaw_tracer() -> &'static Tracer { /* service.name=openpaw */ }
```

Platform code paths (dispatch, actor, WASM host, state-timeouts, admission, Cedar) use `temper_tracer()`. Embodiment code paths (triggers, HTTP routers in `paw-triggers`, app installer, CLI) use `openpaw_tracer()`.

**Why OTel resource overrides**: the alternative — compiling two separate binaries or running two processes — breaks the co-located-by-design nature of OpenPaw-as-embodiment. Resource overrides let one process emit two service identities while keeping the co-location.

### Sub-Decision 2: Metrics split by prefix

- All `temper_*` metrics → `service:temper`. They describe the platform regardless of embodiment.
- All `openpaw_*` metrics → `service:openpaw`. Reserved for embodiment-specific telemetry (trigger failure rates, HTTP routing errors, app-installer progress). Most don't exist yet; as they land, they go under this prefix.
- `temper_up` stays on `service:temper` (it's the canary for the platform).
- Log-derived metrics: `temper.logs.*` for platform logs, `openpaw.logs.*` for embodiment logs.

Implementation: a per-meter resource override, same pattern as tracers. Two `Meter` handles, one per service.

### Sub-Decision 3: Disambiguate instances via `env` + `host`, not a custom tag

The earlier draft of this ADR introduced a custom `embodiment` resource attribute to distinguish Railway from local-Mac Temper instances. That was wrong: Datadog already ships `env` as a first-class resource attribute and auto-tags every span/metric/log with `host`. Together these disambiguate instances without any new concept:

- `env:prod` → the Railway Temper.
- `env:dev` (auto-set by OTel or explicitly set locally) → any Mac instance.
- Multiple local instances in parallel → `env:dev host:<hostname>` disambiguates them. If you need finer control, pass `DD_ENV=dev-<suffix>`.

No `embodiment` tag. No parallel concept to explain. No dashboard filters that mix the two vocabularies. Correction issued 2026-04-20 before any dashboard or monitor shipped using the custom tag.

### Sub-Decision 4: Cross-service trace continuity

A single user-initiated flow traverses both services:

```
Slack webhook receive         (service:openpaw, env:prod)
  └─ trigger.dispatch         (service:openpaw, env:prod)
       └─ Session.Configure   (service:temper,  env:prod)
            └─ wasm.invoke    (service:temper,  env:prod)
                 └─ provider  (service:temper,  env:prod)
```

Trace ID is preserved across service boundaries because both tracers share the same propagation context. Datadog's APM UI renders cross-service traces natively — no special configuration needed on the ingestion side.

### Sub-Decision 5: Dashboard split

`openpaw-overview.json` becomes two files:

- `/Users/seshendranalla/Development/openpaw/dd-dashboards/temper-platform.json` — dispatch resilience, state liveness, actor runtime, admission, WASM host, session context, blob, Monty REPL. The vast majority of today's widgets.
- `/Users/seshendranalla/Development/openpaw/dd-dashboards/openpaw-embodiment.json` — HTTP surface, trigger health, app installer, Katagami-app-level metrics.

Service Catalog entries created for both: `temper` (owned by platform team), `openpaw` (owned by embodiment team — de facto the same people today, but the boundary matters for scale).

### Sub-Decision 6: Monitor re-tagging

`[OpenPaw]` prefix becomes:

- `[Temper]` for platform monitors (dispatch, state-timeout, admission, mailbox, etc.).
- `[OpenPaw]` stays for embodiment monitors (webhook errors, trigger failures).

Queries get retagged: `{service:openpaw}` becomes `{service:temper}` on platform monitors.

## Rollout Plan

1. **Phase 0 (Immediate)** — Land `otel.rs` module with both tracer/meter handles. No call-site changes yet. Deploy, confirm `temper_up` still emits normally.
2. **Phase 1 (+1 day)** — Switch all `temper_*` metric emissions to use the `temper` meter. Existing dashboards continue to work because they query by metric name, not service.
3. **Phase 2 (+3 days)** — Switch all platform spans (`dispatch.*`, `wasm.invoke`, `entity.*`, `registry.*`, `dispatch.state_timeout`) to use the `temper` tracer. Add `embodiment:openpaw` to the resource.
4. **Phase 3 (+1 week)** — Dashboard file split: `temper-platform.json` and `openpaw-embodiment.json`. Deploy via `scripts/deploy_dashboard.py`. Archive old `openpaw-overview.json`.
5. **Phase 4 (+1 week)** — Monitor re-prefix and re-tag. Verify no monitor goes into No-Data during the transition (traffic continuity).
6. **Phase 5 (+2 weeks)** — Tamago deployment: `embodiment:tamago` tag lights up. Service Catalog shows two embodiments under `temper`.

## Readiness Gates

- `temper_up{service:temper}` and `temper_up{service:openpaw}` both emitting during Phase 0.
- Cross-service trace example visible in Datadog APM: one trace spanning Slack webhook (openpaw) → Session.Configure (temper).
- All `[Temper]` monitors have state other than No-Data within 1h of Phase 4 deploy.

## Consequences

### Positive
- Platform SLOs measurable independently of embodiment.
- Service Catalog correctly represents the Temper platform as it spreads to more embodiments.
- Query ambiguity gone — `service:temper resource_name:Session.*` means platform-level Session work, period.
- Tamago deployment has a clean observability story on day one.

### Negative
- Existing dashboards and saved queries need updating. One-time cost.
- Operators learning "is this span `temper` or `openpaw`?" for the first few weeks. Mitigation: consistent naming conventions (dispatch-prefixed spans are always temper, trigger-prefixed are always openpaw).

### Risks
- **Tracer confusion in code review**: engineers may pick the wrong tracer handle for a span. Mitigation: naming convention (platform code in `crates/temper-*` → `temper_tracer()`; embodiment code in `crates/openpaw`, `crates/paw-triggers` → `openpaw_tracer()`) plus a clippy lint if feasible.
- **Cross-service trace breakage**: if propagation context is dropped at a boundary, traces split. Mitigation: integration test that starts a webhook flow, asserts one trace ID across both services.

### DST Compliance
- No determinism impact. Tracers and meters emit on `tokio::spawn` side-channels already (non-deterministic by design; excluded from DST simulation).

## Non-Goals

- Splitting the binary or the repo. OpenPaw-as-embodiment stays co-located with its Temper.
- Inter-service HTTP calls between temper and openpaw. They share the process.
- Different retention tiers per service (revisit after volume baseline is known).
- Renaming crates in the repo to match the service split (crate names are an internal concern; service names are external).

## Alternatives Considered

1. **Single service + `component:temper|openpaw` tag** — Rejected. Tag-based dimensionality does not populate the Service Catalog. As Temper spreads to multiple embodiments, the catalog remains blind to it.
2. **Split the binary (two processes)** — Rejected. Breaks OpenPaw-as-embodiment design; forces network hops between platform and trigger layer. The co-location is intentional.
3. **Separate Datadog accounts per embodiment** — Rejected. Fragmentation makes cross-deployment analysis impossible.
4. **Wait for Tamago to ship, then decouple** — Rejected. Tamago is mid-build. Decoupling before its first deploy is dramatically cheaper than after.

## Rollback Policy

If per-span service overrides misbehave in production:

1. Phase 0–2 reversal: switch both tracer/meter handles to return the same `openpaw` resource. No code-path changes needed.
2. Dashboards revert via `scripts/deploy_dashboard.py` with the archived `openpaw-overview.json`.
3. Monitors revert via the same deploy tool.

Rollback is file-based; no Datadog-side manual work.
