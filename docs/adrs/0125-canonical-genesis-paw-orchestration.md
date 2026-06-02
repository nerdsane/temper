# ADR-0125: Canonical Genesis Apps And Paw Orchestration

- Status: Accepted
- Date: 2026-06-01
- Deciders: Temper core maintainers
- Related:
  - ADR-0124: Directed Evolution Proof Telemetry And Diffs
  - Genesis `apps/directed-evolution`
  - Genesis `temperpaw/paw-orchestration` app ref
  - `crates/temper-platform/src/os_apps`

## Context

Directed Evolution needs to run as a real loop, not as a second set of demo-only files. Review found three architectural gaps:

1. Temper-native app bundles were checked into multiple platform repositories, making source of truth ambiguous.
2. Directed Evolution used domain-specific `BrainRun` execution language even though the same execution machinery is needed by TemperPaw, Paw Patrol, and future agent work.
3. Datadog evidence could be described or linked, but the observer and evaluator paths did not fail closed when mandatory telemetry was missing.

The user-facing model is: Genesis owns app bundles; Temper and TemperPaw own platform/worker code plus pinned Genesis refs; execution provenance is shared worker terminology; and the Directed Evolution observer must inspect Datadog alongside Temper runtime and Genesis history.

## Decision

### Genesis Is Canonical For App Bundles

Production Temper-native apps live in Genesis as pinned app refs. Platform repositories may keep code, install tooling, tests, fixtures, and a tiny immutable first-boot Genesis seed, but they must not act as production app catalogs.

`os-apps/` in platform repositories is therefore a bootstrap/development surface. Runtime startup should install configured pinned Genesis refs and recover installed apps from state. During local development or tests inside the Genesis workspace, Temper prefers the sibling Genesis `apps/` directory over repo-local app fixtures.

### Shared Execution Moves To `temperpaw/paw-orchestration`

The `agent-orchestration` app is renamed and published as
`temperpaw/paw-orchestration`; the bundle-local app name remains
`paw-orchestration`. It becomes the shared execution app and owns:

- `WorkerProvider`
- `WorkerAgent`
- `WorkItem`
- `WorkerRun`

Directed Evolution keeps domain entities such as `Direction`, `Episode`, `Variant`, `Trial`, `StageResult`, and `Promotion`, but execution provenance is represented by `WorkItemId`, `WorkerRunId`, `CreatedByWorkerRunId`, and `SelectorWorkerRunId`.

`BrainRun` is not a Directed Evolution domain concept. UI, prompts, evidence, and entities should use worker/provider/run language.

### Concurrency Is A Temper-Claimed Process Pool

Local Codex execution is modeled as multiple worker processes. Each process registers as a distinct `WorkerAgent` slot and claims one `WorkItem` at a time. Temper distributes work through claims, capabilities, lanes, and exclusive keys.

Selector and promoter lanes use exclusive keys for episode/organism serialization. Observer, simulated-user, and evaluator lanes may run concurrently when their capabilities and exclusive keys allow it.

### Datadog Evidence Is Mandatory Where Declared

The V1 observer is one holistic worker role. It must inspect Datadog, Temper runtime/user evidence, and Genesis/app history before proposing directions. A direction proposal must include structured evidence scope, rejected interpretations, confidence, and Datadog query summaries.

Telemetry evaluation stages fail closed without query, time window, result count, interpretation, zero-result meaning, and a usable Datadog link. Datadog links are secondary evidence; the structured summary is the authoritative Temper record.

## Rollout Plan

1. **Phase 0 (Immediate)** — Add ADRs, rename app vocabulary, introduce/normalize shared worker entities, remove the local Codex single-run architecture constraint, make Datadog evidence validation explicit, and move `directed-evolution` plus `temperpaw/paw-orchestration` into Genesis `apps/`.
2. **Phase 1** — Publish/pin the Genesis refs and replace remaining repo-local production app-bundle usage with pinned install references.
3. **Phase 2** — Run a fresh Agent Answers proof cycle from a clean seed app ref with concurrent worker slots and mandatory Datadog evidence.

## Readiness Gates

- No new production app bundle is added to a platform repository outside the Genesis first-boot bootstrap exception.
- Directed Evolution queues shared worker work and records worker-run provenance instead of `BrainRun` provenance.
- Local Codex workers can run as multiple Temper-claimed process slots.
- Observer/evaluator outputs fail closed when required Datadog evidence is missing.
- Mission Control and Genesis app details show file/code diffs, worker runs, Datadog summaries, and organism genealogy.

## Consequences

### Positive

- Genesis has one clear job: canonical app version history and install refs.
- TemperPaw, Paw Patrol, and Directed Evolution can share execution language and provenance.
- Concurrency becomes visible and controllable through Temper claims instead of hidden inside a worker loop.
- Reviewers can understand why directions were proposed and why variants were eliminated without asking chat.

### Negative

- Existing repo-local `os-apps/` usages need migration to pinned Genesis refs.
- Cross-app choreography must use declared reactions and shared entity references instead of app-local shortcuts.

### Risks

- Moving app source of truth too quickly could break first-boot development workflows. The bootstrap exception exists to keep a minimal recovery path.
- Existing legacy tenants may contain `BrainRun` records. UI should tolerate legacy data while new runs use worker-run language.

### DST Compliance

This ADR does not require new simulation-visible Rust code. Any later changes to `temper-runtime`, `temper-jit`, or `temper-server` must preserve deterministic simulation rules.

## Non-Goals

- Migrating legacy proof tenants.
- Replacing Codex with TemperPaw-native agents for all roles in V1.
- Building a UI chat surface. Chat remains outside Mission Control.

## Alternatives Considered

1. **Keep `BrainRun` in Directed Evolution** — Rejected because it duplicates TemperPaw execution concepts and makes shared worker provenance unclear.
2. **Use one worker daemon with internal multi-run scheduling** — Rejected for V1 because Temper should own distribution, claims, capabilities, and exclusive keys.
3. **Allow GitHub mirrors of app bundles** — Rejected because it leaves reviewers unsure whether Genesis or a repository is authoritative.

## Rollback Policy

Re-enable legacy repo-local app installation only behind an explicit development flag. Do not make repo `os-apps/` a production catalog again without a superseding ADR.
