# Directed Evolution Repair-Aware Variant Lanes Proof

Date: 2026-05-27

## Scope

The episode orchestrator now switches variant lane suggestions to repair-focused
lanes when the episode context is repair pressure. This addresses the live
repair cycle where growth-flavored lanes produced variants that were correctly
eliminated for missing the repair target.

## Verification

```text
cargo test --manifest-path os-apps/directed-evolution/wasm/episode_orchestrator/Cargo.toml --quiet
running 2 tests
2 passed

./os-apps/directed-evolution/wasm/build.sh
signal_observer built
episode_orchestrator built
work_item_result_router built

git diff --check
passed
```

## Live Evidence That Drove The Change

Tenant:

```text
de-live-repair-cycle-20260527081922
```

The cycle reached a terminal failed state:

```text
Episode en-019e6885-f57b-7e90-8a7e-6923e953b5f5: Failed
Generation en-019e6885-f88d-71f2-b37b-8f691f4d797a: Failed
Reason: All variants were eliminated before selection.
```

Evaluator evidence showed prompt drift:

```text
Variant 2 was eliminated because it added intent-capture/product metadata while
the observed pressure was submitted-answer visibility and acceptance
actionability.
```

## Deployment Note

This is hot-loadable Directed Evolution app/WASM behavior. It does not require a
Railway deployment.
