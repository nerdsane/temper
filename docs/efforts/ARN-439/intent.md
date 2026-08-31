# ARN-439 — Intent

Temper CI is slow: ~26 min on PRs, ~34 min on main. temper is a **public** repo, so
standard GitHub runner minutes cost $0 — every paid service is net-new spend, so the free
fixes come first and must be exhausted before any paid runner is considered.

This effort is **step 1: the free fixes**. Step 2 (Blacksmith runner swap) is prepared as a
follow-up diff but not applied without Rita's go.

## Measured bottlenecks (2026-08-29)

- PR critical path: `Tests` job — 21.7 min of `cargo test --workspace`, compiling COLD every run.
- Main critical path: DST `platform-random` full mode — 32.8 min of pure test execution.
- `Bench Build` 28.6 min: inherits `[profile.release] lto=true, codegen-units=1`.

## Root cause of the cold compiles

Nine per-job `actions/cache` entries each carry `target/`, totaling ~11.8 GiB against
GitHub's 10 GB per-repo cache cap. Over the cap, GitHub evicts LRU continuously, so most
jobs miss cache on every run and recompile from cold. No `Swatinem/rust-cache`, no nextest,
full debuginfo, `CARGO_INCREMENTAL` on in CI, no mold linker.

Baseline runs: 33193748615 (main), 33191609032 (PR).
