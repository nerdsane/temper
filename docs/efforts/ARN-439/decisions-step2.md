# ARN-439 — Step 2 decision log (Blacksmith)

### D-S2-1 — Move only the 3 compile-heavy jobs to Blacksmith; light jobs stay on free runners
- **Decision:** Swap `runs-on` to `blacksmith-4vcpu-ubuntu-2404` for Tests, Bench Build, and
  dst-platform-tests only; check / integrity / verify-specs / instrumentation / dst-matrix-setup /
  verification-contract stay on `ubuntu-latest`.
- **Came up because:** Blacksmith minutes are billable; temper is public so GitHub runners are $0.
- **Options:** (a) all jobs on Blacksmith; (b) only the compile-heavy jobs; (c) none.
- **Chose (b) over (a) because:** the light jobs are seconds-to-2-minutes and are not the critical
  path, so paying for them buys nothing; (a) multiplies the bill for no wall-clock gain. Gained:
  the CPU win where it matters (Tests, the main-tail dst suites, bench) at minimal spend. Gave up:
  a few minutes of the light jobs staying on slower free runners (irrelevant to total time).
- **Where:** `.github/workflows/ci.yml` (three `runs-on` swaps).

### D-S2-2 — Sticky disk owns target/; rust-cache keeps ~/.cargo via cache-targets:false
- **Decision:** On the Blacksmith jobs, mount `./target` as a `useblacksmith/stickydisk` and set
  `cache-targets: false` on `Swatinem/rust-cache` so it manages only `~/.cargo`.
- **Came up because:** rust-cache prunes the workspace's own build artifacts, so `target/` never
  stays warm — the reason step 1's Tests warm≈cold. Both tools cannot own `target/` at once.
- **Options:** (a) rust-cache for everything (target stays pruned, no persistence win);
  (b) sticky disk for target/ + rust-cache (cache-targets:false) for deps; (c) sticky disks for
  both target/ and ~/.cargo (drop rust-cache).
- **Chose (b) over (a)/(c) because:** (a) leaves the actual bottleneck (workspace recompile)
  unsolved; (c) would duplicate the dep registry into per-job sticky disks and lose the
  `shared-key: dst` dedup across the dst cells. (b) persists the whole `target/` (the win) while
  deps stay deduped and colocated-fast. Gave up: one more moving part per heavy job.
- **Where:** `.github/workflows/ci.yml` Tests / Bench / dst jobs.

### D-S2-3 — Per-cell sticky-disk keys for the concurrent dst matrix
- **Decision:** The dst target sticky key includes the matrix cell:
  `…-dst-target-${{ matrix.suite }}-${{ matrix.shard_index }}`.
- **Came up because:** sticky disks are exclusive per key, and the dst matrix cells (core, boot,
  consistency, 4 random shards) run concurrently.
- **Options:** (a) one shared dst target key; (b) a key per cell.
- **Chose (b) over (a) because:** a shared key would force the concurrent cells to serialize on the
  one volume, erasing the parallelism the sharding exists for. Per-cell keys give each cell its own
  warm `target/`. Gave up: more sticky volumes (uncapped NVMe, so no budget pressure).
- **Where:** `.github/workflows/ci.yml` dst-platform-tests job.
