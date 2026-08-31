# ARN-439 — Plan (step 1)

## What we are addressing
temper CI recompiles cold on nearly every job because 9 `target/`-carrying caches overflow
GitHub's 10 GB cap and evict each other. Plus: no nextest, full debuginfo, LTO on the bench
compile, an unsharded 33-min DST tail, no mold.

## Expected end state
One temper PR, CI green, that:
- fits all caches under 10 GB (rust-cache, pruned target/, shared dst key) → no eviction;
- cuts the PR `Tests` tail via warm cache + nextest + line-tables-only debug + mold;
- cuts the main DST tail ~33 → ~8-11 min via 4-way seed sharding, full coverage kept;
- cuts Bench Build via LTO/codegen-units off for the compile-only check.
All coverage preserved; developer-local profiles untouched.

## Steps
1. Worktree off up-to-date main; branch `claude/arn-439-ci-speedup`. [done]
2. Design chain in `docs/efforts/ARN-439/`. [done]
3. Edit `ci.yml`:
   - workflow env: `CARGO_INCREMENTAL=0`, `CARGO_PROFILE_DEV_DEBUG`/`_TEST_DEBUG=line-tables-only`.
   - every compiling Linux job: add `rui314/setup-mold@v1`, replace `actions/cache` with
     `Swatinem/rust-cache@v2`.
   - `test` + dst matrix: install `cargo-nextest`, run under nextest; dst matrix shares
     `shared-key: dst`.
   - dst matrix: expand `platform-random` into 4 shard entries; wire
     `TEMPER_DST_SHARD_INDEX`/`_COUNT`.
   - `bench-build`: job env LTO=off, codegen-units=16.
4. Edit `dst_platform_random.rs`: env-driven seed sharding helper applied to all 5 tests.
5. Local validation: `actionlint` on ci.yml, `cargo metadata`/`cargo build --tests` sanity,
   run the sharded test locally to confirm the union equals the full set.
6. Draft PR early; decision log as I go.
7. Freeze head → ping team-lead for the 3/3 cloud panel (do not self-run). Fix findings.
8. Merge, measure post-merge main run, append before/after table to ARN-439.
9. Prepare (not apply) the Blacksmith step-2 diff + Rita's account setup notes.

## Deferred / out of scope
- Blacksmith runner swap (step 2) — prepared, not applied.
- clippy split, merge queue, sccache — explicitly excluded.
- `deploy-observe.yml`, `sdlc-*` — untouched (other efforts own them).
