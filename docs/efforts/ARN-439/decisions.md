# ARN-439 — Decision log

Appended at the moment each call is made. Self-contained for a reader with zero session context.

---

### D1 — Seed-level sharding instead of nextest `--partition` for `dst_platform_random`
- **Decision:** Shard the DST platform-random suite across 4 CI jobs by *seed* (env
  `TEMPER_DST_SHARD_INDEX`/`TEMPER_DST_SHARD_COUNT`, filter `seed % count == index`), not by
  nextest `--partition count:1/4`.
- **Came up because:** ARN-439 item 5 (and the research) suggested `nextest --partition count:4`.
- **Options:** (a) nextest `--partition` — shards at test-case granularity; (b) seed-level env sharding; (c) move full mode to the nightly cron (coverage drops to daily).
- **Chose (b) over (a) because:** `dst_platform_random` is 5 `#[test]` functions that each loop
  their seeds *internally* (no_faults = 100 seeds × 50 ops in full mode; the others 50 × 30;
  determinism 10 × 50 × 2). nextest `--partition` distributes whole test *functions*, so 5 across
  4 partitions gives {2,1,1,1} and the partition holding `no_faults` still carries most of the
  33-min tail — the target (~9-12 min) is unreachable that way. Seed-level sharding divides every
  test's seed loop evenly, so all 4 shards do ~equal work and the union is exactly the original
  seed set (full coverage, zero loss). Rejected (c) because daily coverage is strictly weaker and
  the minutes are free on a public repo. Gained: even balance + guaranteed target. Gave up:
  literal fidelity to the suggested flag (flagged to team-lead + in the PR).
- **Where:** `crates/temper-server/tests/dst_platform_random.rs` (shard helper + 5 call sites);
  `.github/workflows/ci.yml` `dst-platform-tests` matrix.

### D2 — Share one rust-cache across the 4 dst-platform-random shards (and the whole dst matrix)
- **Decision:** Give the `dst-platform-tests` matrix `shared-key: dst` so all its entries restore
  one cache, rather than one cache per matrix cell.
- **Came up because:** rust-cache auto-keys per matrix cell (matrix values fold into the job id),
  which would create 7 dst caches (3 suites + 4 random shards) and re-pressure the 10 GB cap.
- **Options:** (a) per-cell caches (rust-cache default); (b) one `shared-key` for the matrix.
- **Chose (b) over (a) because:** every dst-platform matrix cell compiles the same `temper-server`
  test dependency graph; the shards compile a *byte-identical* binary (only runtime env differs).
  Sharing one cache holds the deps once instead of 7×, keeping the whole repo comfortably under
  10 GB. Concurrent saves to one key are handled by rust-cache (first wins, rest log-and-skip).
  Gave up: a shard could compile its own test binary on top of a peer's cached deps on a warm run
  (negligible vs the deps cost).
- **Where:** `.github/workflows/ci.yml` `dst-platform-tests` job.

### D3 — mold via `make-default: true`, not RUSTFLAGS
- **Decision:** `rui314/setup-mold@v1` with default `make-default: true` (symlinks mold as the
  system linker) rather than setting `RUSTFLAGS=-Clink-arg=-fuse-ld=mold`.
- **Came up because:** two ways to make cargo link with mold.
- **Options:** (a) `make-default: true`; (b) global `RUSTFLAGS`.
- **Chose (a) over (b) because:** RUSTFLAGS folds into the rust-cache key and would need to be set
  on every job consistently or linking breaks; `make-default` needs nothing beyond the one step
  and leaves cache keys clean. temper's `.cargo/config.toml` only sets rustflags for
  `aarch64-apple-darwin` (local macOS), so nothing in Linux CI conflicts.
- **Where:** `.github/workflows/ci.yml` (all compiling Linux jobs).

### D4 — Keep the three observe-gated commands on `cargo test`, not nextest
- **Decision:** Convert the big workspace run and the DST matrix to `cargo nextest run`, but leave
  the three explicit `temper-server --features observe` commands on `cargo test`.
- **Came up because:** those three exist specifically to *guarantee* observe-gated coverage that
  feature unification would otherwise make incidental (per the in-file comment).
- **Options:** (a) convert them to nextest with `-E` name filters; (b) keep them on cargo test.
- **Chose (b) over (a) because:** they are narrow name-filtered runs and are seconds long. A
  slightly-off nextest filter that matches nothing can exit 0 on some versions, silently dropping
  the very coverage guarantee they exist for. cargo test's positional-substring semantics are the
  exact guarantee already relied on. No meaningful time is lost (the tail is the workspace compile,
  not these runs).
- **Where:** `.github/workflows/ci.yml` `test` job.

### D5 — Leave `badges.yml` untouched; Tests-count badge gracefully falls back to source count
- **Decision:** Do not modify `.github/workflows/badges.yml`, even though switching the Tests job
  to nextest changes which code path it uses.
- **Came up because:** `badges.yml` (a separate `workflow_run`-triggered workflow) counts tests by
  grepping the CI log for libtest's `test result:` lines; nextest prints a different summary format.
- **Options:** (a) rework the badge's log parsing to read nextest's `N tests run:` summary;
  (b) leave it and rely on its existing fallback.
- **Chose (b) over (a) because:** `badges.yml` already has a fallback — when the log grep yields 0
  it counts `#[test]` occurrences in source — so the badge still updates, never errors. The only
  effect is the Tests-count badge switches from "runtime passed count" to "source test count"
  (a larger, still-valid number). Reworking gh-log field parsing for a vanity metric (AGENTS.md
  deprioritizes those) is scope creep beyond the team-lead's bound (ci.yml + the dst test file) and
  is untestable without a live run. Flagged to team-lead/Rita as a known minor consequence; a badge
  rework, if wanted, is a separate one-line follow-up.
- **Where:** `.github/workflows/badges.yml` (unchanged); consequence noted here.
