# Developer harness

## Sub-features
The local git hooks and the harness self-report: `scripts/setup-hooks.sh` installs `pre-commit` (staged-file placeholder, spec-syntax and dependency checks; `.claude/hooks/pre-commit.sh`) and `post-commit` (lifecycle markers; `.claude/hooks/post-commit.sh`). `scripts/verification-v1-report.sh` writes the `verification.v1` contract report about the harness itself; `scripts/verification-v1-validate.sh` checks its shape. Full map: `docs/HARNESS.md`. There is no pre-push hook: fmt, clippy, the readability ratchet and the test suite run in CI (ARN-453).

## How to get to it (user POV)
A contributor clones the repo, runs `scripts/setup-hooks.sh` once, and from then on `git commit` refuses placeholders and broken specs before they reach a PR. The self-report tells them which harness pieces are wired.

## Driving it
Only in a fresh clone (a primary checkout carries session markers that change the report). Drive from the repo root:

```bash
bash scripts/setup-hooks.sh                  # expect: pre-commit + post-commit installed, nothing else
ls .git/hooks | grep -v '\.sample$'           # expect exactly: post-commit pre-commit
git commit --allow-empty -m "harness drive"  # exercises pre-commit on a real commit
R=$(bash scripts/verification-v1-report.sh 2>&1 | sed -n 's/.*report written: //p')
bash scripts/verification-v1-validate.sh "$R"
```
When the hook scripts under `.claude/hooks/` change, also run the changed hook directly on a staged change that should be refused (a `todo!()` in a staged `.rs` file) and one that should pass.

## What proves it
`ls .git/hooks` after the installer; the commit output; `overall_result`, `checks_total`, `failed=0`, `blocking_failures=0` from the report; `verification.v1 contract validation: OK` from the validator. In a fresh clone the report warns on the per-session evidence markers (trace, review and alignment markers, push-pending); those warns are expected and non-blocking.

## Gotchas
- A checkout that ran the installer before ARN-453 has a `.git/hooks/pre-push` wrapper whose target no longer exists. Both the installer and the pre-commit hook remove it; a push that fails with exit 127 on `pre-push.sh` means neither has run yet in that checkout.
- The report is a self-report about the harness, not about temper: CI does not run it (removed in ARN-453 for exactly that reason).
