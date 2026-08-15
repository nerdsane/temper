# Task 117 Red-Green proof

Date: 2026-08-15

## Red

Command:

```text
cargo test -p temper-spec --test scoped_spec_bundle
```

Expected failure observed before implementation: unresolved imports for
`BundleErrorCode`, `IoaSourceInput`, `ScopedSpecBundle`, and
`ScopedSpecBundleInput`.

The golden-vector assertion was also introduced with a placeholder and observed
failing before the expected digest was recorded.

## Green

Commands:

```text
cargo test -p temper-spec --test scoped_spec_bundle
cargo test -p temper-spec
cargo clippy -p temper-spec --all-targets -- -D warnings
```

Focused result: 9 passed, 0 failed. Full `temper-spec` result: 267 unit tests,
3 migration differential tests, and 9 scoped-bundle tests passed. Clippy was
clean after splitting the compiler into files below the 500-line ceiling and
using slice-based canonical annotation sorting.

The first sandboxed workspace run reached the Crucible socket integration tests
and failed because local ephemeral binds were denied. Re-running
`cargo test --workspace` with local-network permission passed the entire
workspace, including the 343-second randomized platform DST workload, 679
`temper-server` unit tests, backend/reaction parity, and doctests. After the
source-only file split, the exact final tree passed `cargo test -p temper-spec`
and `cargo clippy --workspace --all-targets -- -D warnings`.
