# ARN-439 — Step 2 (paid runner acceleration): PARKED

Rita's ruling: **paid CI runners are deferred — the tier, not just one vendor.** temper stays under
the `nerdsane` **personal** GitHub account by design (the kernel does not move to an org). Step 1
(the free fixes, PR #447) is merged and delivering; step 2 is parked, not abandoned. This file is
the revival guide.

## Why the obvious vendors don't work here

- **Blacksmith** — does NOT support personal GitHub accounts. Verified empirically: on this branch
  both `blacksmith-4vcpu-ubuntu-2404` and `-2204` jobs sat queued with `runner_name=""` for 44 min
  while GitHub-runner jobs ran fine — no runner is ever offered to a personal-account repo.
- **WarpBuild** — requires a GitHub **organization** (registers runners in the org's Default runner
  group). Same wall, given the nerdsane-stays-personal ruling.

## THE revival path — Namespace (works on personal accounts)

Namespace supports personal GitHub accounts verbatim: *"Namespace Runners now support personal
GitHub accounts as well as organizations"* (namespace.so/blog/completing-the-circle-with-github-runners,
2023-04-27). Revival is one install + a re-wire of the (now-closed) PR #453:

1. **Rita, one-time:** sign up at namespace.so (Developer plan → 30-day free trial), install the
   Namespace GitHub App on the `nerdsane` account, create a workspace and connect `temper`.
2. **Re-wire the 3 heavy jobs** (Tests, Bench Build, dst-platform-tests) — light jobs stay on free
   GitHub runners:
   - `runs-on: nscloud-ubuntu-24.04-amd64-4x8-with-cache`
     (plus `nscloud-cache-tag-<job>` and `nscloud-cache-size-20gb` labels; per-cell tag for the
     concurrent dst matrix, same reasoning as the sticky-disk keys in the closed #453 diff).
   - Add `namespacelabs/nscloud-cache-action@v1` to mount the persistent cache volume at `/cache`,
     and point cargo `target/` (and `~/.cargo`) into it — this is the warm-`target/` win (rust-cache
     prunes workspace artifacts; the volume keeps them, which is what step 1 could not do and why
     the Tests job was warm≈cold).
   - Keep mold, nextest, the CI profile env, seed-sharding, and the dynamic matrix unchanged.
3. **Benchmark on the free trial** ($0): first run (cold volume) + second (warm), vs step 1's
   post-merge free-fix baseline; extend the ARN-439 table with a Namespace column and compute
   $/month from actual billed minutes.

## Cost model (to confirm from a trial run)

Namespace 4 vCPU Linux: **$0.004/runner-min prepaid** ($0.006 overage); no permanent free tier, but
a 30-day free trial. Modeled for temper (heavy jobs only on Namespace): **~$30-40/mo** + negligible
cache storage. The closed **PR #453** carries the exact 3-job structure (runs-on swap + per-cell
cache keys + cache-action) to adapt from — swap Blacksmith labels/stickydisk for the nscloud
equivalents above.

The design rationale (which 3 jobs, why per-cell cache keys, why volume-for-target + rust-cache-for-
deps) is in `decisions-step2.md` and transfers directly to Namespace.
