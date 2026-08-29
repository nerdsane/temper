# ARN-439 — Spec (step 1: free CI fixes)

Optimize `nerdsane/temper` CI **without a paid service**. One PR, all six fixes, no punting.
Only `.github/workflows/ci.yml` and `crates/temper-server/tests/dst_platform_random.rs` are
touched. `deploy-observe.yml` (being deleted by another effort) and the `sdlc-*` workflows
are out of scope.

## Contract

Coverage is preserved exactly. Nothing that runs today stops running. The wins are wall-clock
and cache-fit only. Developer-local build profiles are untouched (all profile changes are
CI-scoped via workflow/job env).

## The six fixes

1. **Cache layer** — replace all nine per-job `actions/cache` blocks (registry + git + `target/`)
   with `Swatinem/rust-cache@v2`, which prunes `target/` to dependency artifacts, sets
   `CARGO_INCREMENTAL=0`, and auto-keys per job. The four `dst-platform` matrix entries that
   compile the identical `temper-server` test dep graph share one cache via `shared-key: dst`.
   The `Tests` job — which deliberately excluded `target/` — gets a rust-cache too. Result:
   total cache fits under 10 GB, evictions stop.

2. **CI cargo profile** — workflow-level env `CARGO_INCREMENTAL=0`,
   `CARGO_PROFILE_DEV_DEBUG=line-tables-only`, `CARGO_PROFILE_TEST_DEBUG=line-tables-only`.
   Smaller debuginfo → faster codegen + link. CI-only; no `Cargo.toml` profile edits.

3. **cargo-nextest** — install via `taiki-e/install-action@v2` (`tool: cargo-nextest`); run the
   workspace test suite and the DST matrix under `cargo nextest run`. `--skip dst_` becomes the
   filterset `-E 'not test(dst_)'`. (No runnable doctests exist in the tree — all doc fences are
   `text`/`toml`/`sql`/`ignore` — so nextest, which does not run doctests, loses zero coverage.)

4. **Bench Build** — job-level env `CARGO_PROFILE_BENCH_LTO=off`,
   `CARGO_PROFILE_BENCH_CODEGEN_UNITS=16`. CI only compiles benches (`--no-run`), never runs
   them, so LTO and single-codegen-unit optimization are pure waste there.

5. **Shard `dst_platform_random` full mode ×4** — the main tail. Implemented as **seed-level
   sharding** (not nextest `--partition`): the 5 test functions loop seeds internally, so
   test-case partitioning cannot balance them. The test reads `TEMPER_DST_SHARD_INDEX` /
   `TEMPER_DST_SHARD_COUNT` and each shard runs `seed % count == index`. The matrix expands
   `platform-random` into 4 shard entries. Union of shards = every seed → full coverage.
   Smoke mode on PRs is unchanged (still `TEMPER_DST_RANDOM_MODE=smoke`).

6. **mold linker** — `rui314/setup-mold@v1` (`make-default: true`) on every Linux job that
   compiles, so all linking uses mold with no RUSTFLAGS change.

## Proof

Baseline vs after, per-job wall clock, from this PR's own runs (first run seeds caches = cold;
a trivial follow-up commit gives the warm-cache run). `gh cache list` total must fit under
10 GB after. Both cold and warm numbers reported honestly.
