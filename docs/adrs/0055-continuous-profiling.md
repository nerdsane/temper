# ADR-0055: Continuous Profiling for the Temper Runtime

- Status: Accepted
- Date: 2026-04-18
- Deciders: Temper core maintainers
- Related:
  - ADR-0052: Instrumentation as policy
  - ADR-0053: Datadog service decoupling
  - `nerdsane/temper#146`: Entity-actor dispatch contention on cold-start under concurrent bursts (the incident this ADR exists to unblock)
  - `nerdsane/temper#147`: Handler-level deadline propagation
  - `crates/temper-server/src/profiling.rs` (to be created)
  - `crates/temper-server/src/main.rs` (profiler init)
  - `crates/temper-server/Cargo.toml` (add `datadog-profiling` dep)

## Context

The 2026-04-18 Katagami incident exposed dispatch contention on entity-actor cold-start. Under an 11-entity concurrent File-create burst, some actor asks exceeded a 15-second retry budget, surfacing as `actor dispatch exhausted after 3 attempt(s)`. The investigation identified five plausible bottlenecks:

1. Actor-registry mutex contention on cold-spawn.
2. Event-store writer lock / fsync serialization.
3. Synchronous OTel span export on the dispatch hot path.
4. Idempotency-cache first-touch warm-up.
5. Cedar policy lazy-compilation.

Traces and metrics can narrow this to two or three candidates but cannot pick the culprit. The standard industry answer — continuous profiling — is not wired. Specifically:

- No `datadog-profiling` crate dependency in `temper-server`.
- No `@runtime-id` tag on spans, which is what Datadog's Profiler uses to correlate profiles to traces.
- No Profiler dashboard or alerting.

Without profiles, temper#146 is blocked on guesswork. Beyond that incident, any future performance regression in the Temper runtime is similarly dark.

## Decision

Enable Datadog Continuous Profiler on every Temper deployment, gated by an env flag, with CPU + wall-clock profiles on by default and heap in canary only. Wire `@runtime-id` into spans so profile-to-trace stitching works automatically.

### Sub-Decision 1: Crate choice — `pprof` + custom Datadog uploader

**Status correction (2026-04-18)**: the crates.io `datadog-profiling` package is an empty placeholder (`"An empty Datadog crate — Head over to docs.datadoghq.com"`). Datadog does not ship a production-ready Rust profiling SDK today. We therefore go with what was listed as the fallback immediately.

**Primary**: `pprof-rs` (`pprof = "0.14"`) for CPU + wall-clock sampling. Produces pprof-format profiles.

**Uploader**: a small in-repo component that POSTs the pprof blob to the Datadog Agent profile intake endpoint (`/profiling/v1/input`) with Datadog's first-party multipart shape: a profile attachment such as `cpu.pprof` plus an `event` part named `event.json`. The event carries `service`, `env`, `version`, `runtime-id`, `runtime`, and `profile.component` tags in `tags_profiler`.

**Why this path**: pprof-rs is mature (used by tikv and much of the tokio-console ecosystem). The Datadog intake endpoint format is documented and stable. Building the uploader is lower total maintenance than waiting for the Datadog Rust SDK to ship.

**Revisit**: if Datadog publishes a real Rust SDK, migrate in a follow-up ADR.

### Sub-Decision 2: Profile types

- **CPU**: 60-second windows, 100 Hz sampling rate. Catches hot code under burst load. Low overhead (~1% p99 CPU).
- **Wall-clock**: 60-second windows. Captures time-in-syscall, I/O wait, lock wait — the actual signal needed for dispatch contention (most of that wait is *not* on-CPU). Overhead ~1–2%.
- **Heap**: sampled allocations. Enabled in staging + canary only; disabled in prod by default due to 5–10% overhead at high allocation rates.

Configurable via env:
- `TEMPER_PROFILING_CPU=true` (default).
- `TEMPER_PROFILING_WALL=true` (default).
- `TEMPER_PROFILING_HEAP=false` (default; set to `true` in staging).

### Sub-Decision 3: Tags

Profiler emissions carry, at minimum:

- `service` — per ADR-0053 (`temper` for platform profiles; `temperpaw` for TemperPaw deployments when they embed the platform).
- `env` — `prod`, `staging`, `dev`.
- `version` — the binary's `BUILD_VERSION` env (already set in the Dockerfile).
- `host` — auto.
- `runtime-id` — UUID generated at process start, persisted for process lifetime. Matches the `@runtime-id` on spans so Datadog can stitch profiles to traces.
- `profile.component` — `cpu` | `wall` | future profile families. Lets the UI filter profiles to the captured runtime component without creating a second service identity.

### Sub-Decision 4: `@runtime-id` on spans

One-line change in tracer initialization (`crates/temper-server/src/otel.rs` per ADR-0053):

```rust
let runtime_id = Uuid::new_v4().to_string();
Resource::new(vec![
    KeyValue::new("service.name", "temper"),
    KeyValue::new("runtime-id", runtime_id),
    // ... existing attributes
])
```

The same `runtime_id` is passed to the profiler init so profiles and spans carry identical values. Datadog's APM ↔ Profiler stitching is automatic once the IDs match.

### Sub-Decision 5: Runtime controls

Master switch: `TEMPER_PROFILING_ENABLED` (default `false` until post-canary).

Upload timeout: `TEMPER_PROFILING_UPLOAD_TIMEOUT_MS=5000`. If the Datadog Agent is slow, the profiler does not backpressure the application — it drops the upload and continues.

Agent endpoint: defaults to `http://127.0.0.1:8126` (Railway's Datadog Agent). Overridable via `DD_AGENT_HOST` / `DD_TRACE_AGENT_PORT`.

### Sub-Decision 6: Observability of the profiler itself

The profiler is telemetry. It emits its own metrics:

- `datadog.profiling.rust.profiles_uploaded{service:temper}` — counts successful uploads. Expected rate: 1 per 60s per replica per enabled profile type.
- `datadog.profiling.rust.upload_errors{service:temper}` — failures.
- `datadog.profiling.rust.overhead_ms{service:temper}` — self-reported overhead per cycle.

Dashboard widget: `Profiler Status` panel on the Temper platform dashboard (per ADR-0053 split). Shows uploads/min, errors, and a link to APM → Profiles filtered to `service:temper`.

Monitors:

- `[Temper] Profiler Upload Failures` — `sum(last_15m):datadog.profiling.rust.upload_errors.as_count() > 5`.
- `[Temper] Profiler Uploads Stalled` — `sum(last_10m):datadog.profiling.rust.profiles_uploaded.as_count() < 1` (per replica). Routed to Slack, severity medium.

### Sub-Decision 7: Canary plan

- **Week 1 (canary)**: enable on one Railway replica (CPU + wall). Measure p99 dispatch latency delta against a control replica; target: <2% regression. If >5%, investigate before fleet rollout.
- **Week 2 (fleet)**: flip `TEMPER_PROFILING_ENABLED=true` at the service level. All replicas profile.
- **Week 3 (diagnostic run)**: run the 100-way File-burst harness from temper#146 with profiler on. Expect wall-clock flame graph to point directly at the contended lock / append path. Update temper#146 with the finding.
- **Week 4 (staging heap)**: enable heap profiling in staging only. Validate overhead, confirm memory-pressure questions can be answered without prod heap profiles.

## Rollout Plan

1. **Phase 0 (Immediate)** — Add `datadog-profiling` dep, create `profiling.rs`, wire `@runtime-id`. Default disabled. Land and deploy.
2. **Phase 1 (+1 day)** — Canary replica flipped on. Measure overhead.
3. **Phase 2 (+1 week)** — Fleet-wide enablement.
4. **Phase 3 (+2 weeks)** — File-burst diagnostic run; temper#146 investigation concludes with flame-graph evidence.
5. **Phase 4 (+3 weeks)** — Staging heap enabled; memory-pressure incidents get first-class profiles.

## Readiness Gates

- `datadog.profiling.rust.profiles_uploaded{service:temper}` emitting ≥1/min per replica at Phase 2.
- p99 `temper_dispatch_ask_latency_ms` on canary within 2% of control replica.
- Profile visible in Datadog APM → Profiles → `service:temper`; flame graph loads.
- `@runtime-id` present on ≥99% of spans (proven via `aggregate_spans` grouped by `@runtime-id`).

## Consequences

### Positive
- temper#146 unblocked with direct evidence of contention source.
- Any future Temper perf regression gets investigated with profiles, not guesses.
- Profile-to-trace stitching lets operators go from a slow span directly to its CPU flame graph.
- Foundation for future work: memory leak detection, GC / allocator tuning.

### Negative
- ~1–2% p99 CPU overhead at steady state. Accepted.
- One more external dependency (`datadog-profiling` crate).
- Upload traffic to Datadog Agent (~1 profile per 60s per type per replica; bounded).

### Risks
- **Crate maturity**. The Rust profiling crate is newer than Go/Java equivalents. Mitigation: canary, fallback to `pprof-rs`.
- **Overhead regression** in a future dependency update. Mitigation: `Profiler Overhead Regression` monitor; env-flag kill switch.
- **Profile-to-trace stitching silently breaks** if `@runtime-id` drift between process restarts leaks into the UI. Mitigation: regenerate `runtime-id` on process start only; document that restart breaks stitching across the restart boundary.

### DST Compliance
- Profiler runs outside simulation-visible code paths. CPU sampling uses timer signals; wall-clock sampling uses thread state. Neither is part of DST simulation. Profile data is not replayed.
- `runtime-id` uses `Uuid::new_v4()` which is non-deterministic; acceptable because the id is a tracking tag, not a simulation variable. Tagged `// determinism-ok: observability-only ID`.

## Non-Goals

- Flame-graph-guided auto-tuning. Profiles inform humans, not a scheduler.
- Cross-language profile merging. Temper is Rust-only; WASM guest profiling is out of scope (handled separately under WASM tracing).
- Custom pprof endpoints. Datadog is the only profiling backend.
- Profiling of the macOS-bundled Tamago embodiment at v1. Tamago deployment profiling is Phase 5+ work.

## Alternatives Considered

1. **`pprof-rs` with custom Datadog uploader** — deferred to fallback role. Official crate is lower total maintenance.
2. **Profiling via eBPF on Railway hosts** — rejected. Railway's platform doesn't expose eBPF access to user services.
3. **Enable profiling on demand only (reactive)** — rejected. The contention we need to diagnose is bursty; without continuous profiles, we miss the window.
4. **Sample traces at 100% as a substitute for profiles** — rejected. Traces show span boundaries, not on-CPU sampling inside a span; they cannot identify hot code paths.

## Rollback Policy

Primary: set `TEMPER_PROFILING_ENABLED=false` at the service level. Profiler threads shut down; overhead drops to zero within the next sampling window.

Secondary: remove the `datadog-profiling` dep via a revert PR. `runtime-id` on spans remains (harmless; it's just a tag).

Tertiary: keep the runtime-id plumbing indefinitely even if the profiler is disabled — it's free insurance for any future profiling backend.
