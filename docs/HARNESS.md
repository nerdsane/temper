# Temper development checks

Shared workflow is owned by Stack. [REVIEW.md](../REVIEW.md) gives the repository review criteria; review and evidence are advisory and use plain language.

The active checks are:

- `.claude/settings.json`: spec verification, determinism and dependency checks while editing.
- `scripts/setup-hooks.sh`: local pre-commit spec/dependency/code checks and pre-push compile, lint and test checks.
- `.github/workflows/ci.yml`: build, lint, tests and the kernel's runtime invariants.

Run relevant checks for the changed behavior and report the results and any skipped coverage. There are no review markers, proof packets, planning-document checks or session-exit gates.
