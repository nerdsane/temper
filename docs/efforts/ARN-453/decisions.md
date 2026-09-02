# ARN-453 — Decisions

**Decision:** Delete the badges workflow and make the README badges dynamic instead of repairing the gist pipeline.
**Came up because:** badges.yml failed on an expired gist token, and the README never displayed the three badges it generated; its own Rust/version badges were hardcoded (Rita: badges must be accurate).
**Options:** (a) rotate the token, keep gist badges the README ignores; (b) delete the workflow, switch the README to shields.io dynamic-TOML badges reading Cargo.toml.
**Chose (b) over (a) because:** accuracy by construction with zero moving parts (no workflow, no secret, no drift); dropped the test-count and crate-count badges as vanity metrics.
**Where:** README.md badge block; badges.yml removed; BADGE_GIST_ID var + GIST_SECRET deleted.

**Decision:** Delete the verification-contract CI job rather than adapt it to the missing pre-push hook.
**Came up because:** removing the pre-push hook broke the job's expectations; auditing it showed it validates a self-report of hook installation on the CI runner.
**Options:** (a) adapt the checks; (b) delete the job, keep the scripts as a local harness self-report.
**Chose (b) over (a) because:** the job carried no signal about temper's code — it tested the installer.
**Where:** ci.yml; docs/HARNESS.md contract section.

**Decision:** Bench compile moves to a separate weekly+dispatch workflow instead of a schedule inside ci.yml.
**Came up because:** the nightly schedule is being removed; a weekly schedule inside ci.yml would run every job weekly and need if-gates on all of them.
**Options:** (a) weekly schedule in ci.yml with if-gates; (b) manual-only; (c) its own small workflow.
**Chose (c) over (a)/(b) because:** distinct trigger = distinct file (same reasoning as decision-intake); manual-only rots silently.
**Where:** .github/workflows/bench.yml.

**Decision:** Consolidate the four gate workflows into one file with four jobs; workflow-level concurrency per PR.
**Came up because:** Rita's ruling; four files each cloning stack.
**Options:** (a) one job with four steps (one clone); (b) one workflow, four jobs.
**Chose (b) over (a) because:** branch protection requires four separate check contexts; (a) would collapse them to one. Runtime is unchanged (each job is its own runner); the win is one file to read and vendor. Re-run scripts that referenced sdlc-review / sdlc-verification by workflow name now target the combined workflow (a rerun re-executes all four gates — acceptable, they are cheap).
**Where:** stack gates/sdlc.yml (canonical), temper .github/workflows/sdlc.yml; stack proof/post-proof-record.sh, gates/sdlc-decision-intake.yml.

**Decision:** DST matrix cells select binaries by exclusion (`binary(/^dst_/) and not ...`) instead of listing them; the Tests job excludes the dst_ binaries instead of test names containing `dst_`.
**Came up because:** review (codex) found three dst_ integration binaries (entity_key_index, entity_vector_index, genesis_install_rollback) that no matrix cell ran while the Tests job skipped every test named `dst_*`; the deleted pre-push hook's full `cargo test --workspace` had been the only place they ran. The name filter also dropped lib unit tests with `dst_` in their name.
**Options:** (a) add the three binaries to the core cell; (b) define cells by exclusion so an unlisted binary cannot exist.
**Chose (b) over (a) because:** (a) fixes three instances and leaves the next new dst_ binary uncovered again.
**Where:** .github/workflows/ci.yml Tests step and dst-matrix-setup.

**Decision:** Only the review job keeps a concurrency group; the workflow-level group was removed.
**Came up because:** review (fable) noted the four singles had no shared group; a shared cancel-in-progress let an `edited` event cancel a proof render mid-Vercel-deploy.
**Options:** (a) shared group without cancel; (b) job-level group on review only, as before.
**Chose (b) over (a) because:** it is the previous behavior exactly; the other three jobs are seconds long and idempotent.
**Where:** sdlc.yml review job (temper and stack).

**Decision:** Stale pre-push wrappers are removed by both the installer and the pre-commit hook itself.
**Came up because:** review (codex, fable) - a checkout that ran the old installer has `.git/hooks/pre-push` exec'ing a script this PR deletes, so its next push fails with exit 127. Reproduced locally.
**Options:** (a) tell people to re-run setup-hooks.sh; (b) installer removes it; (c) also self-heal from the tracked pre-commit hook, which every such checkout already runs.
**Chose (c) over (a)/(b) because:** it needs no human step; the commit that precedes the push removes the wrapper.
**Where:** scripts/setup-hooks.sh, .claude/hooks/pre-commit.sh.
