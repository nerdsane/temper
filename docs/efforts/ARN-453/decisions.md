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

**Decision:** The Tests job's exclusion is scoped to temper-server (`not (package(temper-server) and binary(/^dst_/))`) instead of making the core matrix cell workspace-wide.
**Came up because:** round 2 (codex) - the matrix only runs temper-server, so a dst_ binary in another crate would have been excluded by the Tests job and run nowhere.
**Options:** (a) core cell `--workspace` (compiles every crate's tests in that cell too); (b) scope the Tests-job exclusion to the package the matrix covers.
**Chose (b) over (a) because:** complete coverage in both directions at no extra compile; other crates' dst_ binaries run in the Tests job like any other test.
**Where:** .github/workflows/ci.yml Tests step.

**Decision:** Matrix exclusions are anchored regexes (`/^dst_platform_boot$/`), and the generator's `-E` expressions are JSON-escaped double quotes inside the single-quoted SUITES string.
**Came up because:** round 2 - fable found the single-quote nesting was a bash syntax error (CI's matrix setup failed on 1afb3a99; I had pushed a `run:` block without executing it), and the bare `binary(dst_platform_boot)` exclusion was a substring match.
**Options:** none for the syntax error; for the match, bare vs anchored.
**Chose anchored because:** a future `dst_platform_boot_replay` binary would otherwise be silently dropped from platform-consistency.
**Where:** ci.yml dst-matrix-setup. The generator step is now executed locally before push, and a `workflows-lint` CI job (actionlint) makes the class of error un-mergeable.

**Decision:** Add an actionlint job to CI.
**Came up because:** the quoting bug above; actionlint catches it (verified against the broken head), and I had run it only before the matrix edit.
**Options:** (a) rely on running actionlint by hand; (b) a seconds-long CI job.
**Chose (b) over (a) because:** a rule I already had and skipped once is not a control; the job is.
**Where:** .github/workflows/ci.yml job workflows-lint.

**Decision:** The installer resolves the hooks directory against the repository the script lives in (`git -C "$WORKSPACE_ROOT"`), not the caller's cwd.
**Came up because:** round 3 (codex, fable) - my round-2 worktree fix used the cwd's repository, so running the installer by absolute path from another repo would have written temper's hooks there. Reproduced, then driven from a foreign cwd after the fix (foreign repo untouched, temper hooks installed).
**Options:** (a) `cd "$WORKSPACE_ROOT"` at the top; (b) `git -C`.
**Chose (b) because:** one expression, no cwd side effect for the rest of the script.
**Where:** scripts/setup-hooks.sh.

**Decision:** Remove the `evidence.push_post_verify` self-report check and describe the post-push hook as what it is: a `push-completed` trace marker, no tests.
**Came up because:** round 3 (codex) - the report looked for `push-pending`/`test-verified` markers that nothing writes; HARNESS.md said the post-push hook runs `cargo test`. Both were fiction; the check could only ever "skip".
**Options:** (a) make the hook write the markers the report wants (re-adding push-time tests locally); (b) delete the check and correct the docs.
**Chose (b) over (a) because:** CI verifies pushed code; a local test run at push time is the thing this effort removed.
**Where:** scripts/verification-v1-report.sh, docs/internal/verification.v1.mapping.md, docs/HARNESS.md (overview diagram, hook row, Component 6, marker flow, lifecycle diagram).

**Decision:** The actionlint installer script is fetched from the v1.7.7 tag, not main; the lint and bench jobs declare `contents: read`; bench.yml installs the toolchain from rust-toolchain.toml instead of a second hard-coded pin.
**Came up because:** round 3 (fable) - an unpinned script piped to bash in a job with the default token; duplicated nightly pin.
**Options:** none worth recording for the pin; for the toolchain, (a) duplicate the env var, (b) let rustup read the file.
**Chose (b) because:** the file is already the source of truth for developers.
**Where:** .github/workflows/ci.yml workflows-lint, .github/workflows/bench.yml.

**Decision:** Branch protection is out of scope for this PR and surfaced to the owner instead.
**Came up because:** round 3 (fable) asked whether required contexts were updated. Checked: nerdsane/temper main has NO branch protection and no rulesets (GitHub API 404 / empty), so no context is required and none blocks a merge today.
**Options:** (a) add a ruleset from this session; (b) report it.
**Chose (b) because:** repository settings are the owner's call and were never part of this effort's intent; the gates are enforced by process, not by GitHub, until she decides.
**Where:** PR #454 completion report.

**Decision:** ADR-0079 amended in place rather than superseded.
**Came up because:** round 3 (codex) - the ADR still required the nightly and bench-on-every-non-PR-event.
**Options:** (a) a new ADR; (b) an amendment section.
**Chose (b) because:** the rest of ADR-0079 (PR smoke mode, seed shards, concurrency) still stands.
**Where:** docs/adrs/0079-ci-pr-smoke-and-dst-matrix.md.
