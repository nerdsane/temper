# ARN-439 — Step 2: Blacksmith runner (implemented in this PR)

Step 1 (merged, PR #447) exhausted the free fixes. Rita approved moving the compile-heavy jobs
onto Blacksmith runners for bare-metal CPU and a colocated cache, plus a sticky disk that
persists `target/` across runs — the one thing rust-cache cannot do (it prunes the workspace's
own artifacts), which is why the `Tests` job was warm≈cold in step 1.

Blacksmith is a drop-in runner: change the `runs-on` label, keep the rest of the workflow.

## What this PR changes

Only the three compile-heavy jobs move; the light jobs stay on free GitHub runners (public
repo → $0, and they are not the bottleneck):

| Job | Runner | target/ | ~/.cargo |
|---|---|---|---|
| Tests | `blacksmith-4vcpu-ubuntu-2404` | sticky disk `…-tests-target` | rust-cache (`cache-targets: false`) |
| Bench Build | `blacksmith-4vcpu-ubuntu-2404` | sticky disk `…-bench-target` | rust-cache (`cache-targets: false`) |
| dst-platform-tests | `blacksmith-4vcpu-ubuntu-2404` | sticky disk `…-dst-target-<suite>-<shard>` (per cell) | rust-cache (`shared-key: dst`, `cache-targets: false`) |
| check, integrity, verify-specs, instrumentation, dst-matrix-setup, verification-contract | `ubuntu-latest` (free) | rust-cache | rust-cache |

mold, cargo-nextest, the CI profile env, seed-sharding and the dynamic matrix all carry over
unchanged — Blacksmith runs the same steps, faster.

## Why sticky disk for target/ AND rust-cache for ~/.cargo

`Swatinem/rust-cache` prunes `target/` to dependency artifacts and drops the workspace's own
compiled crates, so a large workspace recompiles itself every run — this is exactly why step 1's
`Tests` warm run (17.3m) barely beat its cold run (16.3m). A Blacksmith **sticky disk** mounted at
`./target` is a persistent NVMe volume that keeps the *whole* target directory warm across runs,
so the workspace's own test binaries survive. `cache-targets: false` tells rust-cache to stop
managing `target/` (the sticky disk owns it) and cache only `~/.cargo` deps — no double-management,
no conflict. Deps still dedupe across the dst cells via `shared-key: dst`.

Sticky disks are exclusive per key, and the dst matrix cells run concurrently, so each cell gets a
distinct target key (`…-dst-target-<suite>-<shard_index>`); sharing one key would serialize them.

## The gate (must clear before merge)

The `blacksmith-*` runner labels resolve only once the **Blacksmith GitHub App is installed on
nerdsane/temper**. That is a browser step for Rita:
1. https://blacksmith.sh → sign in with GitHub as `nerdsane`.
2. Install the Blacksmith app on the `temper` repo.
3. (Optional, for free minutes) apply to the open-source program — temper is public.

Until then, any `runs-on: blacksmith-*` job sits queued with no runner. **Do not merge** until a
Blacksmith-labelled job actually picks up — verified with a `workflow_dispatch` smoke run on this
branch (a queued-forever job means the app is not installed yet).

## Measurement plan

Once a Blacksmith run executes: capture the first run (cold sticky disk) and a second run (warm
sticky disk) per job, compare against step 1's post-merge free-fix baseline, and extend the
ARN-439 table with a third column. Compute realistic $/month from actual billed Blacksmith minutes
(3,000 free min/mo offset; the OSS program may zero it). Adopt only if the delta justifies the
spend; otherwise step 1's $0 result stands and this PR is dropped.
