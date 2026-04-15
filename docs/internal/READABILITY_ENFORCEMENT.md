# Readability Enforcement

This document defines how Temper keeps Rust readability standards from regressing.

## Scope

- Workspace path: `crates/**/*.rs`
- Production files exclude: `*/tests/*`, `*/benches/*`, `*_test.rs`, `test_*.rs`, `*/temper-macros/*`

## Enforcement Layers

1. Formatting: `cargo fmt --check`
2. Linting: `cargo clippy --workspace -- -D warnings`
3. Integrity checks: `bash scripts/integrity-check.sh`
4. Readability ratchet: `bash scripts/readability-ratchet.sh check .ci/readability-baseline.env`

## Ratchet Metrics

The ratchet blocks regressions on:

- `PROD_FILES_GT500`
- `PROD_FILES_GT1000`
- `PROD_MAX_FILE_LINES`
- `ALLOW_CLIPPY_COUNT`
- `ALLOW_DEAD_CODE_COUNT`
- `PROD_PRINTLN_COUNT`
- `PROD_UNWRAP_CI_OK_COUNT`

The ratchet tracks but does not block on:

- `PROD_FILES_GT300` (advisory only, to avoid penalizing healthy splits)

## Commands

Show current metrics:

```bash
bash scripts/readability-ratchet.sh show
```

Create/update baseline:

```bash
bash scripts/readability-ratchet.sh snapshot .ci/readability-baseline.env
```

Check against baseline:

```bash
bash scripts/readability-ratchet.sh check .ci/readability-baseline.env
```

## Baseline Update Policy

Baseline changes should be rare. A PR that increases any ratcheted metric must include:

1. Reason the increase is necessary
2. Plan to reduce the metric
3. Owner and target PR/milestone

## Weekly Hygiene

At least once per week, run:

```bash
bash scripts/readability-ratchet.sh show
bash scripts/integrity-check.sh
```

Track trend in PR notes or project board until:

- all active feature branches are merged
- oversized file count is trending down
- suppression counts are stable or decreasing
