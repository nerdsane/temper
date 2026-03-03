# verification.v1 Mapping (Temper Harness)

This document maps the current Temper harness to model-agnostic `verification.v1` check IDs.

- Schema: `docs/verification.v1.schema.json`
- Generator: `scripts/verification-v1-report.sh`
- Default report output: `/tmp/temper-harness/{project_hash}/verification.v1-{session}.json`

## Usefulness Scale

- `useful`: high signal, low ambiguity, meaningful enforcement.
- `limited`: useful but conditional, spoofable, or noisy.
- `not_useful`: currently weak enough that it should not be trusted for gating.

## Hardness Dimensions

- `accidental_regression`: catches unintentional mistakes.
- `adversarial_bypass`: resists deliberate gaming.
- `portability`: can be consumed by any model/runtime with minimal adaptation.

Scores are normalized to `[0.0, 1.0]`.

## Check IDs

| verification.v1 id | Source | Stage | Blocking | Evidence class | Usefulness | Accidental | Adversarial | Portability | Notes |
|---|---|---|---|---|---|---:|---:|---:|---|
| `config.hook.pretool.plan_reminder` | `.claude/settings.json` + `check-plan-reminder.sh` | `config` | No | `mechanical` | limited | 0.20 | 0.05 | 0.70 | Advisory nudge only. |
| `config.hook.posttool.verify_specs` | `.claude/settings.json` + `verify-specs.sh` | `config` | Yes | `mechanical` | useful | 0.90 | 0.45 | 0.60 | Strong when configured. |
| `config.hook.posttool.check_deps` | `.claude/settings.json` + `check-deps.sh` | `config` | Yes | `mechanical` | useful | 0.85 | 0.50 | 0.65 | Uses `cargo tree` graph evidence. |
| `config.hook.posttool.check_determinism` | `.claude/settings.json` + `check-determinism.sh` | `config` | Yes | `heuristic` | limited | 0.70 | 0.30 | 0.65 | Regex scan, suppressible comments. |
| `config.hook.pretool.pre_commit_review_gate` | `.claude/settings.json` + `pre-commit-review-gate.sh` | `config` | Yes | `mechanical` | useful | 0.88 | 0.40 | 0.55 | Strong structure, marker freshness weak. |
| `config.hook.posttool.post_push_verify` | `.claude/settings.json` + `post-push-verify.sh` | `config` | No | `mechanical` | limited | 0.60 | 0.35 | 0.60 | Advisory + marker coordination. |
| `config.hook.stop.stop_verify` | `.claude/settings.json` + `stop-verify.sh` | `config` | Yes | `mechanical` | limited | 0.75 | 0.30 | 0.55 | Good fallback, commit-marker path currently unwired. |
| `config.hook.posttool.trace_capture` | `.claude/settings.json` + `trace-capture.sh` | `config` | No | `mechanical` | useful | 0.55 | 0.40 | 0.80 | Portable trace artifact, limited tamper coverage. |
| `install.git_hook.pre_commit` | `.git/hooks/pre-commit` wrapper | `git` | Yes | `mechanical` | useful | 0.82 | 0.50 | 0.80 | Catches non-Claude commit paths. |
| `install.git_hook.pre_push` | `.git/hooks/pre-push` wrapper | `git` | Yes | `mechanical` | useful | 0.80 | 0.48 | 0.80 | Catches non-Claude push paths. |
| `install.git_hook.post_commit` | `.git/hooks/post-commit` wrapper | `git` | Yes | `mechanical` | useful | 0.74 | 0.42 | 0.82 | Wires stop-gate commit lifecycle markers. |
| `evidence.trace.integrity` | `trace-*.jsonl` + `verify-trace.sh` | `trace` | No | `mechanical` | useful | 0.65 | 0.55 | 0.85 | Hash chain on selected fields. |
| `evidence.marker.review.dst` | `dst-reviewed(.toml)` marker | `review` | Conditional | `attestation` | limited | 0.55 | 0.20 | 0.75 | Presence-only unless freshness checked. |
| `evidence.marker.review.code` | `code-reviewed(.toml)` marker | `review` | Conditional | `attestation` | limited | 0.55 | 0.20 | 0.75 | Presence-only unless freshness checked. |
| `evidence.marker.alignment_reviewed` | `alignment-reviewed(.toml)` marker | `review` | Yes | `attestation` | limited | 0.60 | 0.25 | 0.70 | Semantic review, still marker-based gating. |
| `evidence.push_post_verify` | `push-pending-*` vs `test-verified-*` markers | `push` | Yes | `mechanical` | useful | 0.68 | 0.30 | 0.75 | Effective when marker lifecycle is intact. |
| `wiring.exit_gate.commit_markers` | `stop-verify.sh` plus post-commit marker writers | `wiring` | Yes | `mechanical` | useful | 0.72 | 0.35 | 0.82 | `commit-pending`/`sim-changed` are written by git post-commit hook. |
| `wiring.marker.session_binding` | `pre-commit-review-gate.sh` / `stop-verify.sh` | `wiring` | No | `inferred` | limited | 0.35 | 0.10 | 0.85 | Marker existence checked; session/change binding not enforced. |
| `wiring.pre_push_determinism_blocking` | `pre-push.sh` + `scripts/check-determinism.sh` | `wiring` | Intended Yes | `mechanical` | limited | 0.30 | 0.10 | 0.80 | Determinism scan currently advisory in practice. |

## Universal Consumption Contract

Any model can consume `verification.v1` if it can parse JSON. The producer is adapter-specific, but the output schema is provider-agnostic:

1. Run adapter checks (Claude hooks, git hooks, CI artifacts, marker scans).
2. Normalize each result into `checks[]` with canonical `id`, `result`, and `evidence_class`.
3. Compute summary + hardness aggregates.
4. Emit one `verification.v1` JSON document.

For Temper, step 1-4 are implemented by `scripts/verification-v1-report.sh`.
