# ARN-439 — Step 2: Blacksmith runner (prepared, NOT applied)

Step 1 (this PR) exhausts the free fixes. Step 2 moves the compile-heavy jobs onto Blacksmith
runners for bare-metal speed and a colocated 4x cache. It is **not applied** — it lands only
after Rita's go and a warm-vs-cold benchmark on a trial, because temper is public and every paid
minute is net-new spend.

Blacksmith is a drop-in runner: you change the `runs-on` label and keep the rest of the workflow.
Because step 1 uses `Swatinem/rust-cache` (not a Blacksmith-specific cache action), the cache layer
needs **no change** — on a Blacksmith runner the same rust-cache transparently reads/writes
Blacksmith's colocated cache, which is faster and not bound by GitHub's 10 GB cap. So the step-2
diff is almost entirely a label swap.

## What Rita must set up first (one-time)

1. Sign up at https://blacksmith.sh and install the **Blacksmith GitHub App** on the `nerdsane`
   org (or just the `temper` repo). No secrets, no self-hosted infra — the App provisions runners
   on demand.
2. Apply to the **open-source program** (temper is public, so it likely qualifies for free or
   heavily discounted minutes). Until that clears, the free tier (3,000 min/mo + a colocated
   Actions cache well above 10 GB) already covers a lot.
3. Nothing else. No AWS account, no runner maintenance.

## The diff (apply only after her go)

Swap `runs-on: ubuntu-latest` → `runs-on: blacksmith-4vcpu-ubuntu-2404` on the three
compile-heavy jobs. Leave the light jobs (verification-contract, integrity, instrumentation) on
free GitHub runners — they are not the bottleneck and burn no meaningful minutes.

```diff
   test:
     name: Tests
-    runs-on: ubuntu-latest
+    runs-on: blacksmith-4vcpu-ubuntu-2404
```
```diff
   dst-platform-tests:
     name: DST/Platform Tests (...)
-    runs-on: ubuntu-latest
+    runs-on: blacksmith-4vcpu-ubuntu-2404
```
```diff
   bench-build:
     name: Bench Build
-    runs-on: ubuntu-latest
+    runs-on: blacksmith-4vcpu-ubuntu-2404
```

`check` (Compile & Lint) is the next candidate if PR wall-clock is still gated by it after the
above — swap it too and re-measure.

## Optional: sticky disk for `target/` (only if rust-cache colocated isn't enough)

If the colocated rust-cache restore is still a visible cost on the heaviest jobs, mount `target/`
(and `~/.cargo`) as a Blacksmith **sticky disk** — a persistent volume hot-loaded into the runner,
better than any cache action when the artifact set is very large. This replaces the rust-cache
step *on Blacksmith jobs only*:

```yaml
      - name: Mount cargo sticky disks
        uses: useblacksmith/stickydisk@v1
        with:
          key: ${{ github.repository }}-cargo-registry
          path: ~/.cargo
      - name: Mount target sticky disk
        uses: useblacksmith/stickydisk@v1
        with:
          key: ${{ github.repository }}-${{ github.job }}-target
          path: ./target
```

Sticky disks require a Blacksmith runner (they no-op / fail on GitHub runners), so this is strictly
a post-swap option. Keep one key per job (per-job `target/` differs); the dst matrix can share one
key the way step 1 shares `shared-key: dst`.

## Benchmark before committing to paid

On the trial, run the workflow twice on a Blacksmith runner (cold then warm) and compare the
`Tests` and `platform-random` wall-clocks against step 1's warm numbers. Adopt Blacksmith only if
the delta justifies the spend; otherwise step 1's $0 result stands. Namespace (native `cache: rust`
volumes, ~$30-50/mo) is the managed fallback if Blacksmith's OSS terms don't land.
