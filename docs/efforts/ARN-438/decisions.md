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
