# Decision log - temper side of ARN-438

Appended as each call was made. The PR body's `## Decisions & Tradeoffs` carries
these verbatim.

---

**Decision:** Present the cascade as five levels but flag L2b Actor Simulation as
defined-but-not-CLI-wired, rather than claiming actor-level DST from `temper verify`.
**Came up because:** the brief said "extend spec-cascade to 5 levels," and the
source (`temper-verify/src/cascade.rs`) has L0/L1/L2/L2b/L3, but no caller passes
`with_actor_sim`, so the CLI runs L0/L1/L2/L3.
**Options:** (a) list five levels as if all run in the CLI; (b) list five and state
L2b is not wired, pointing actor-level DST at the `dst_*` suites; (c) list only the
four the CLI runs.
**Chose (b) over (a)/(c) because:** (a) would be a false claim a reader could not
reproduce (violates groundedness); (c) hides a real level that exists and matters.
(b) is accurate and tells the reader where actor-level DST actually comes from.
Given up: a tidy "all five run" story.
**Where:** `features/spec-cascade.md`; `features/dst-proof.md`.

---

**Decision:** Refute the "blobs/TemperFS 503 when not on turso" gotcha and document
the 503s by their real error codes instead.
**Came up because:** the brief carried that gotcha, but source shows both turso and
postgres stacks provide query_plane + metadata + the blob API.
**Options:** (a) keep the turso-specific gotcha as given; (b) replace it with the
source-accurate transient/config 503s.
**Chose (b) over (a) because:** the turso claim is false against this checkout
(`storage/mod.rs` `from_turso`/`from_postgres` both populate the planes); shipping
it would mislead. Encoding the correction is the AGENTS.md "corrections become
constraints" rule.
**Where:** `features/blobs-and-temperfs.md`. Also surfaced to the lead.

---

**Decision:** Cite `registry` + `temper-jit/swap.rs` for spec hot-swap and
explicitly warn that `temper-evolution` is NOT it.
**Came up because:** the natural guess (a crate literally named "evolution") is
wrong - `temper-evolution` is the GEPA prompt-evolution engine for agents.
**Options:** (a) map hot-swap to temper-evolution; (b) map it to the real registry
swap path and warn off the misnomer.
**Chose (b) over (a) because:** (a) is factually wrong; the swap primitive is
`SwapController::swap` in temper-jit, wired through the registry and the
`/api/specs/load-*` routes. The warning saves the next reader the same wrong turn.
Given up: nothing.
**Where:** `features/spec-hot-swap.md`.

---

**Decision:** Make SKILL.md's launch recipe set an isolated `TURSO_URL` per
instance and `--storage turso`.
**Came up because:** the brief's recipe named `--storage turso`, and the shared
default db is the boot stack-overflow we already hit (ARN-435).
**Options:** (a) leave the recipe backend-agnostic; (b) pin `--storage turso` + an
isolated `TURSO_URL`, explaining the shared-default hazard.
**Chose (b) over (a) because:** `TURSO_URL` unset defaults to
`~/.local/share/temper/agents.db` (`serve/bootstrap.rs`), which another session may
be using; isolation is the difference between a clean boot and a corrupt one. Given
up: brevity in the launch block.
**Where:** `.agents/skills/verify-temper/SKILL.md` (Launch).

---

**Decision:** Delete `deploy-observe.yml` outright with no replacement.
**Came up because:** Rita ruled it misleading (skips on missing secrets, exits
green), and item 4 makes temper's deploy leg the temperpaw pin.
**Options:** (a) fix it to fail loudly on missing secrets; (b) delete it.
**Chose (b) over (a) because:** temper has no direct prod deploy - fixing it would
harden a leg that should not exist. The real deploy path is the pin-bump PR into
temperpaw. Given up: nothing (no environment consumed this workflow's output).
**Where:** `.github/workflows/deploy-observe.yml` (removed).

---

**Decision:** #448 panel round-1 accuracy pass - fixed six real recipe inaccuracies
against source, and rebutted the seventh (dst_genesis_install_rollback) with
evidence rather than "fixing" a correct doc.
**Came up because:** the panel spot-checked citations; for a feature map an
inaccurate recipe is a first-class bug (agents drive these and get false failures).
The research subagents had introduced semantic errors I passed through.
**Options:** (a) apply all seven verbatim; (b) verify each against source, fix the
real ones, and push back on any that are wrong.
**Chose (b) over (a) because:** groundedness beats deference - blindly "fixing" a
correct doc injects a NEW error. Verified: dst_genesis_install_rollback.rs exists on
origin/main (git ls-tree) and the temper-server dst_* count is exactly 13, matching
the table - so act-on 1 is a false finding, left as-is. Fixed the six real ones:
SKILL mkdir -p .scratch; cedar decisions ?status=pending (DecisionStatus is
serde lowercase); query-surface entity reads need the bearer while /tdata,/tdata/,
/tdata/$metadata are public (edge.rs); integrations restated to observable (no
universal retry/DLQ/callback promise); blobs $value is Blob-binary-only, overflow
reads via the entity; README maps verify-remote. Re-audited every undriven file's
routes against source - all real.
**Where:** `.agents/skills/verify-temper/` (SKILL.md, features/{cedar-authz,
query-surface,integrations-and-webhooks,blobs-and-temperfs,entity-lifecycle}.md,
features/README.md).

---

**Decision:** #448 panel round-2 (10 act-ons) - fixed by DRIVING against a live boot
+ source, per the owner's method change, and corrected the docs to observed reality.
**Came up because:** round 2 expanded (7->10) as fresh eyes mined different files;
that convergence-breaker signal meant "stop editing text, drive the recipes."
**Options:** (a) keep text-editing per finding; (b) boot temper, drive what's
drivable, source-confirm the rest, correct to reality.
**Chose (b) because:** it's the only way to stop the expansion. Driven/confirmed and
fixed: invalid-from-state dispatch is **409 Conflict** (bindings.rs:266-328,
effects.rs:366), NOT 200 (my round-0 doc had the pg-actor path's behavior);
VERIFIED_OPERATOR_WHEN uses `principal.agentTypeVerified` not `context`
(engine/mod.rs:732); TURSO_PLATFORM_URL is checked before TURSO_URL so the isolation
recipe must unset it (bootstrap.rs:59/90); entity-set names are the CSDL
EntitySet.Name (measured pluralized-but-irregular: Policy->Policies) - read from
$metadata, don't hand-pluralize; only contains/startswith/endswith are supported
filter functions (filter_sql.rs:427); inbound webhook is HMAC->Cedar->dispatch
(receiver.rs:135); the ADR-0048 503+Retry-After is a dispatch Transient, not a
blob-read 503 (bindings.rs:359); /healthz+/version are temper-platform not
temper-server. Measured live: entity reads 401 unauth / 403 Cedar-denied for the
bootstrapped operator key (so create/dispatch on system entities isn't drivable
locally). The `temper decide` CLI queries ?status=Pending vs lowercase-stored
decisions -> filed as ARN-442; doc points at the curl flow with ?status=pending.
**Where:** `.agents/skills/verify-temper/` (SKILL, cedar-authz, entity-lifecycle,
query-surface, integrations-and-webhooks, blobs-and-temperfs, README); `.gitignore`
(.scratch/). Evidence: `/tmp/verify-temper/<date>/driven-round3.txt`.

---

**Decision:** #448 HOLD-1/HOLD-2 after Rei held `e14370a` — isolate the
serve-and-odata drive recipe, and drop the false `/version` claim.
**Came up because:** agents hit `features/serve-and-odata.md` first (feature-map
row 1) and it still shipped `cargo run -p temper-cli -- serve --port 3600` with
no isolate, while SKILL.md on the same head unsets `TURSO_PLATFORM_URL` and uses
`TURSO_URL=file:$PWD/.scratch/temper.db` — that is the ARN-435 shared-db crash.
Separately, README claimed unauthenticated `/healthz` and `/version` on
`build_platform_router`; only `/healthz` is registered (`router.rs:28`).
**Options:** (a) point serve-and-odata at SKILL; (b) copy SKILL's isolate recipe
into the driving block. For `/version`: (c) add the route; (d) drop the claim.
**Chose (b)+(d) because:** a pointer still lets an agent copy the local bash
block and crash; the drive recipe has to be safe on its own. `/version` has no
caller and no test — adding a product route to make a doc true is the wrong
direction; `/healthz` on the platform router stays (Rei already verified that).
**Where:** `.agents/skills/verify-temper/features/serve-and-odata.md`;
`features/README.md`.

---

**Decision:** #448 round 3 (breaker triggered, 9 codex act-ons) - fixed the 9 against
source and froze; owner re-armed the breaker for ONE bounded verification round.
**Came up because:** 7->10->9 across rounds is the convergence breaker - a 13-file
map has an unbounded textual-accuracy tail (each round mines different files). Owner
decision: fix these concrete 9, then a bounded verify-the-9-only round; new findings
below Important pre-adjudicated to validate-through-use.
**Options:** (a) keep panelling; (b) fix the 9, freeze, bounded verify, RESOLVE any
dispute, merge - no further rounds.
**Chose (b) because:** the owner's call; the map self-corrects at first real use.
Fixed (source-verified): dst_ must select by --test BINARY not a fn-name filter;
Content-Type headers on the POST examples (cedar-authz, spec-hot-swap, entity-lifecycle);
load-inline `specs` map MUST include model.csdl.xml (load_dir.rs:230); turso index
maintenance is NOT atomic co-commit - key-index unmaintained-on-write, vector-index
write-behind (event_store.rs:140-176), postgres co-commits; the table is
`wasm_invocation_logs` (schema.rs:122); **WASM "never dispatches" is a paw-patrol
CONTRACT, not a host impossibility** - the host has http_call and can reach the
governed /tdata (load-bearing for stage 3); HMAC signs the FULL request target incl
/webhooks/<tenant> (receiver.rs:90); create retains ordinary initial fields (only
server-derived stripped, write.rs).
**Where:** `.agents/skills/verify-temper/` (dst-proof, cedar-authz, spec-hot-swap,
entity-lifecycle, event-sourcing-readback, wasm-integration, integrations-and-webhooks).

---

**Decision:** Integrate commit 9453ba0f (serve-and-odata isolation + dropping the
false /version-on-platform-router claim) into #448 by rebasing onto it, keeping it.
**Came up because:** it landed on `claude/arn-438-verify-temper` mid-round-3. It is
authored under the shared `rita-aga` identity by a CONCURRENT session (HOLD-1/HOLD-2
message, another checkout of this worktree) - NOT the team lead, who confirmed they
never pushed to this branch. Attribution corrected here so the log does not credit
the wrong actor.
**Options:** (a) force-push over it; (b) rebase onto it and keep it.
**Chose (b) because:** its content is correct and verified - the serve isolation
recipe matches SKILL.md (unset TURSO_PLATFORM_URL), and dropping the /version claim
matches the live 404 probe (/version does not exist until the parked /version PR
adds it). Force-pushing would silently discard a valid concurrent fix. Only
decisions.md needed a both-entries merge.
**Where:** `features/serve-and-odata.md` (from 9453ba0f); this entry.
