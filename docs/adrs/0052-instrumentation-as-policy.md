# ADR-0052: Instrumentation as Policy

- Status: Proposed
- Date: 2026-04-18
- Deciders: Temper core maintainers
- Related:
  - ADR-0048: Dispatch retry and error taxonomy
  - ADR-0049: State-entry timeouts and durable scheduler
  - ADR-0050: Mandatory liveness coverage
  - ADR-0051: Admission control in dispatch
  - ADR-0053: Datadog service decoupling (counterpart)
  - ADR-0054: Log standard (counterpart)
  - ADR-0055: Continuous profiling (counterpart)
  - `crates/temper-server/src/runtime_metrics.rs` (all metric registrations)
  - `/Users/seshendranalla/Development/openpaw/dd-dashboards/openpaw-overview.json`
  - `/Users/seshendranalla/Development/openpaw/dd-monitors/openpaw-monitors.json`

## Context

ADRs 0048, 0049, 0050, and 0051 shipped four platform primitives (dispatch retry, state-entry timeouts, mandatory liveness, admission control). Each was designed with observability up front — metric names pre-declared, dashboard widgets and monitor JSON specified in the ADR. Despite that, the 2026-04-18 audit found:

- **33 of 49 live `temper_*` metrics are unreferenced** by any widget or monitor. Entire primitive families (dispatch errors, state-timeout firings, mailbox utilization) are emitted but unalerted.
- **9 metrics are registered in `runtime_metrics.rs` but never emitted** — e.g., `temper_admission_active_permits`, `temper_admission_queue_depth`, `temper_admission_permit_hold_time_ms`, `temper_actor_mailbox_full_drop_total`, `temper_actor_ask_reply_latency_ms`, `temper_scheduler_pending_timers`, `temper_scheduler_overdue_on_replay_total`, `temper_spec_liveness_violations_total`, `temper_spec_allow_indefinite_states`. The registrations look complete in PRs; the wiring slipped.
- **All 8 production monitors return No Data.** Four target a `openpaw.*` namespace that was never emitted. Three target `trace.http.request.*` but Temper spans emit as `trace.custom`. One has a gauge-scoping bug.
- **Dashboards ran in a half-wired state for weeks** because no CI gate catches the divergence between code and config.

The gap is not design — every ADR planned the observability correctly. The gap is **enforcement**. A new primitive cannot ship with incomplete instrumentation and expect anyone to catch it in review. We need the platform itself to refuse incomplete PRs and to refuse stale monitor queries.

## Decision

Make instrumentation a first-class review artifact, enforced by CI and by a standing cleanup loop.

### Sub-Decision 1: PR checklist (human-enforced, reviewer-required)

Every PR that adds or modifies a platform primitive (anything in `/crates/temper-server/src/state/`, `/crates/temper-server/src/entity_actor/`, `/crates/temper-runtime/src/`) must include:

1. **Reserved metric names**, declared in the ADR that accompanies the primitive, with units and tag lists.
2. **Emission sites wired**, not just registered. Every registration in `runtime_metrics.rs` must have at least one call site in the same PR.
3. **Dashboard widget diff** — JSON delta against the Temper / OpenPaw dashboards so the widget ships with the code.
4. **Monitor diff** — JSON delta against the monitor file, or an explicit `<!-- no monitor: <reason> -->` comment in the PR description.
5. **Log lines for expected failures** — if the primitive can fail in a user-visible way, a structured log is emitted with `@error.kind` set to a stable identifier.

Reviewers block the PR if any of the above are missing. This is a hard rule, not guidance.

**Why a checklist rather than automation alone**: the *semantic* question of whether a metric is meaningful cannot be automated. A linter can confirm that every registration has an emission site; it cannot confirm that the emission is on the right code path. Human review closes that gap.

### Sub-Decision 2: CI lint — unemitted registrations fail build

A CI job runs on every PR:

```
1. Parse `crates/temper-server/src/runtime_metrics.rs` for every metric registration.
2. `grep -r` the codebase for each metric name.
3. Fail if a registration has zero emission sites.
```

Implementation: a small Rust binary `crates/temper-server/src/bin/check_instrumentation.rs`, plus a `check-instrumentation` step in the CI pipeline. Runs in seconds.

**Why**: this is the mechanical gap the audit caught — 9 metrics registered, zero call sites. A dumb grep catches 100% of that class of bug.

### Sub-Decision 3: Monitor freshness — No-Data > 7 days triggers cleanup

A scheduled job (cron or GitHub Action, daily) queries Datadog for every monitor tagged `team:openpaw`. Any monitor in `NO DATA` state for 7 consecutive days opens a GitHub issue in `nerdsane/openpaw` with labels `observability` and `monitor-stale`. The issue body includes the monitor name, its current query, and links to the last 30d of the referenced metric in Datadog.

The author of the issue (or the on-call engineer) must do one of:
- Wire the missing emitter (with a follow-up PR).
- Delete the monitor if the concept is no longer relevant.
- Rewrite the query against an equivalent live metric.

Issues stay open as a tracking item until resolved.

**Why 7 days**: short enough to catch stale configs quickly; long enough to ride through a no-traffic weekend without noise.

### Sub-Decision 4: Metric-name discipline

- Platform metrics: `temper_*`.
- Per-embodiment metrics (OpenPaw triggers, HTTP surface): `openpaw_*`. Aligned with ADR-0053's service split.
- Log-derived metrics: `openpaw.logs.*` or `temper.logs.*`.
- No other prefixes. If a PR needs a new namespace, the ADR must justify it.

Namespaces are reserved up-front in the ADR. A PR that emits `temper.new_thing_total` without the ADR reserving that name fails review.

### Sub-Decision 5: Dashboard and monitor files are the source of truth

`/Users/seshendranalla/Development/openpaw/dd-dashboards/` and `/Users/seshendranalla/Development/openpaw/dd-monitors/` are the canonical definitions. `scripts/deploy_dashboard.py` reconciles the Datadog state to match these files. Any manual change in the Datadog UI is reverted on the next deploy.

**Why**: manual-edit drift is how stale monitors accrete. File-first keeps review in Git.

## Rollout Plan

1. **Phase 0 (Immediate)** — Human checklist in effect on next PR. Reviewers reject PRs missing any of the five items.
2. **Phase 1 (This sprint)** — `check_instrumentation` binary lands in CI as a non-blocking warning job. Every unemitted-registration warning is triaged within the week.
3. **Phase 2 (+2 weeks)** — CI gate flips to blocking. Unemitted registrations fail the build.
4. **Phase 3 (+4 weeks)** — Scheduled monitor-freshness job lives in GitHub Actions. First cleanup pass opens issues for all existing No-Data monitors.
5. **Phase 4 (+6 weeks)** — Retrospective: measure PR review overhead, monitor count trend, incident MTTR. Adjust thresholds if needed.

## Readiness Gates

- `check_instrumentation` binary green on `main` at Phase 2.
- Zero Datadog monitors in No-Data > 7d state by end of Phase 3.
- Audit pass by end of Phase 4 confirms every `temper_*` registration has an emission site.

## Consequences

### Positive
- Platform primitives ship with observability as a contract, not an afterthought.
- On-call MTTR drops — every primitive that fails does so with a named metric, a widget, and (usually) a monitor.
- No-Data monitor drift dies. Either a monitor works or it gets closed out.
- New engineers joining the platform have a clear expectation of what "done" means.

### Negative
- Per-PR overhead grows by ~30–60 minutes for primitive work. Tradeoff accepted.
- Monitor / dashboard review becomes part of code review. Some reviewers will need to learn Datadog JSON schema. Mitigation: doc links in the PR template.

### Risks
- **Checklist theater**: reviewers may rubber-stamp the five items without looking. Mitigation: scheduled cleanup job ensures downstream reality-check; the CI lint catches the mechanical class of failure regardless.
- **CI false positives**: an emission site hidden behind feature-flag cfg'd code might trip the grep. Mitigation: the binary honors `#[cfg(...)]` by following rustc's macro-expanded output.

### DST Compliance
- No runtime behavior change. ADR-0052 is a policy ADR; it does not touch simulation-visible crates.

## Non-Goals

- Auto-generating dashboards from code (tried at scale in other orgs; brittle and low-value).
- Replacing Datadog with a different backend (out of scope per the audit plan).
- Enforcing log-message uniformity beyond schema (that's ADR-0054).
- SLO definition (separate initiative after volume baseline settles).

## Alternatives Considered

1. **Per-ADR manual checklist without CI** — rejected. The mechanical failure mode (registered but unemitted) is exactly what CI is for. A human checklist alone would miss it again.
2. **Auto-prune No-Data monitors** — rejected. Automatic deletion hides bugs; issue tracking keeps humans in the loop on whether to wire the emitter or delete.
3. **Per-team dashboards** — rejected. One service, one canonical dashboard (per ADR-0053's split: `temper-platform` and `openpaw-embodiment`). Multiple dashboards per team fragment signal.

## Rollback Policy

- Phase 2 CI gate can be flipped back to non-blocking via a one-line job config change if false-positive rate exceeds 5%.
- Monitor-freshness job can be paused via GitHub Actions UI.
- The policy ADR itself stays even if the tooling rolls back; the checklist is the durable artifact.
