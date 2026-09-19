# Implementation and delivery

Stack AGENTS.md owns scope and authorization. Keep the accepted outcome through implementation, verification and delivery. Workflow guidance is advisory; it does not add approval or evidence gates to CI, hooks or Temper.

## Workspace and delivery

- Use an isolated worktree from the current default branch, named `<agent>/<task>`. Preserve unrelated and uncommitted work. State the repository, worktree, branch and remote host before changing them.
- Stage explicit paths. Commit and push the intended changes; never include secrets or another session's scratch files.
- GitHub operations default to **rita-aga** unless the user selects another account. Verify the login and repository permissions. Scope existing credentials to this task's processes, without printing them or switching the shared global account.
- Open PRs when there is a useful change to inspect. An effort can have multiple PRs in the same or different repositories. Link them to the same Effort. Order delivery by actual dependencies.
- Use concise conventional commit and PR titles. Explain the problem, resulting behavior, relevant verification and remaining limits. Include a durable design note only when it helps future work; do not duplicate it into a required PR-body schema.
- Carry authorized delivery through relevant checks, merge, installation or deployment, and verification of the delivered revision. A request for research or review alone does not authorize a release.
- Genesis remains the current source of truth for TemperPaw and Katagami apps. When delivering an app change, use its existing publish/install route and verify the installed pinned ref. Updating an Effort is not an app deployment.

## A small task record

At the start of a new task session, create or reuse one existing Temper Effort for the accepted objective. On continuation, reuse it. Include the objective, useful context or intent, all PR links, and results or evidence. Use open, done or cancelled to describe reality. Close it when the requested outcome is delivered; closing does not trigger deployment.

Read the installed Effort contract before calling it. No Intent submission, document-readiness checks, review quorum, proof packet or sequence through specification, planning and deployment is required for ordinary task tracking. If tracking is unavailable, retain the objective in the session, report the limitation and continue authorized work. Respect an actual denial on an operation; do not evade it or build a replacement tracker. Update existing linked issues when useful; a second tracker is not a prerequisite.

Keep `intent.md` when it makes the objective easier to understand. Add a spec, plan or decision note only when the change benefits from one. There is no required four-document chain, GitHub attachment ceremony or fixed artifact layout. Preserve meaningful contracts and invariant tests for systems that have them.

## Verification and review

Run the relevant build, lint, type and test checks. Exercise the changed behavior with a suitable observation: a real user flow, API response, CLI output, simulator invariant or installed instruction. Scale the checks to the change and its plausible effects. Use existing tools; do not require every feature to be retested for every edit.

For a meaningful feature, prefer a fresh verifier who reads the code and independently exercises the changed behavior. For a small change, direct verification may be enough. Review is advisory: no mandatory panel, provider, quorum, JSON record, hosted report or owner waiver. Do not commission panels to debate abstract plans; test consequential uncertainty with a small experiment.

[REVIEW.md](../REVIEW.md) is the shared review contract. A repository's root REVIEW.md adds only its concrete domain checks and useful test commands. The interrogate skill describes how to use that contract. Assess findings against the accepted goal; fix demonstrated defects introduced or worsened by the change. Recheck the affected behavior after fixes. Do not restart full reviews of unchanged code or expand the task to satisfy speculative concerns.

Evidence can be a concise PR comment, command and result, screenshot or recording. Identify the revision and disclose what could not be tested. Never label an unperformed review or test as passing. CI should check software behavior and security, not whether the agent completed paperwork.

## Environment and access

Work on the session's host by default. Use a remote environment only when selected by the user or required by a concrete dependency. Do not provision a computer merely to track an Effort. For Tensorlake work, copy the selected source runtime for the session; do not share a writable runtime between sessions.

When moving work, coordinate with its active owner and preserve commits, staged and unstaged files, task scratch files and evidence. Verify the transferred state before removing the source. Stop only task-owned processes.

Check the existing supported route before declaring a capability unavailable. Report the precise failed operation, what can still proceed and the smallest missing access or decision. A missing review service or tracking connection does not block independent implementation and verification. Production verification uses the relevant running environment and available observability; no particular dashboard is a ceremony.

Before claiming delivery, inspect the installed revision and changed behavior. Open a handed-over URL with the intended access. Report failures and material limits directly, and repair or roll back a broken release within the existing authorization.
