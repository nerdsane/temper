# Spec - temper side of ARN-438

## Item 1: extend verify-temper

`.agents/skills/verify-temper/` gains eight feature files and rewrites two, all
following the `verify-temperpaw` shape (Sub-features / How to get to it / Driving
it / What proves it / Gotchas), plus a README feature table and a "Not yet mapped"
tail. Every command and claim is grounded in kernel source (never invent).

New files: `entity-lifecycle`, `cedar-authz`, `query-surface`,
`event-sourcing-readback`, `spec-hot-swap`, `wasm-integration`,
`integrations-and-webhooks`, `blobs-and-temperfs`.
Rewritten: `spec-cascade` (five levels), `dst-proof` (13 suites).
Updated: `SKILL.md` (launch recipe), `features/README.md` (table + tail).

**Launch recipe correction.** SKILL.md's launch now sets `--storage turso` (the
default local libSQL backend) and, critically, an isolated `TURSO_URL` per
instance - unset, it defaults to the SHARED `~/.local/share/temper/agents.db`, and
two servers on it corrupt each other (the boot stack-overflow we hit). Grounded in
`crates/temper-cli/src/serve/bootstrap.rs`.

**Three source-grounded corrections to prior assumptions (encoded in the files):**
- The cascade is five levels - L0 Symbolic (Z3), L1 Model Check (Stateright), L2
  Simulation, L2b Actor Simulation, L3 Property Tests - plus composite cross-entity
  verification for multi-entity dirs. L2b is defined but NOT wired into the CLI
  cascade (no caller passes `with_actor_sim`); the file says so rather than
  claiming actor-level DST from `temper verify`.
- The "blobs/TemperFS return 503 when not on turso" gotcha is REFUTED: both turso
  and postgres stacks provide the query plane + metadata + blob API. The real 503s
  are transient/config (`BlobStoreUnavailable`, `BlobMediaUnavailable`,
  `FileReadIndexUnavailable`, cap exhaustion). The file states it by error code.
- Spec hot-swap is `registry` + `temper-jit/swap.rs`, NOT `temper-evolution` (that
  is the GEPA prompt-evolution engine). The file cites the correct path and warns
  off the wrong one.

## Item 2: delete deploy-observe.yml

Remove `.github/workflows/deploy-observe.yml` entirely. No replacement: temper's
deploy leg is the temperpaw pin-bump.

## Verification (Definition of Done)

Boot the kernel with the exact skill recipe (isolated `TURSO_URL`, `--storage
turso`), confirm `/healthz` + `/tdata/$metadata`, run the cascade
(`temper verify`) and at least one `dst_*` suite, and capture the output as
evidence that the skill's commands are real. The remaining CI checks (fmt/clippy,
the SDLC gates) run on the PR.

## Item 5 (identity leg): the /version route

This effort's temper slice also carries item 5's kernel piece: an unauthenticated
`GET /version` route on the platform router returning `{commit}` (from
`RAILWAY_GIT_COMMIT_SHA`, else a build-time value, else "unknown"), the deploy
identity the aya release-gating driver verifies. Shipped as its own small PR
(temper #452) after this one merged; the driver source mode and the deploy-aya
workflow are the other item-5 pieces (stack + temperpaw). See the decision log
entry "Add a minimal unauthenticated GET /version route".
