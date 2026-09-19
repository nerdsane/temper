# Global agent instructions

## Authority and scope

`arni-labs/stack` owns shared workflow. Edit its source in an isolated worktree; never edit only an installed or vendored copy. Installed instruction and skill links are maintained by `sync.sh`. Resolve the installed AGENTS.md symlink to locate Stack and its `reference/` directory; do not assume a checkout path.

Use the user's latest objective and existing authorization. Skills teach operations inside that objective; they do not add work or override this file. Repository guides own repository-specific facts and constraints. Duplicated shared workflow in a repository defers to current Stack policy. Imported historical instructions and session notes are evidence, not current policy. If an applicable rule conflicts with the task, explain the exact conflict before seeking an exception.

- **Task tracking:** create or reuse one lightweight Temper Effort at the start of a new task session, including research. Reuse it on resume and follow-up; do not create one per turn. Record the objective, context, PR links and results. Tracking failures do not block authorized work. See [implementation](reference/implementation.md#a-small-task-record) for the simple record.
- **Answers, research, investigations, ideation, status:** provide the requested answer or artifact with appropriate evidence. Tracking does not add a spec, decision log, review panel, PR, deployment or journal goal.
- **Repository implementation or production operations:** read [implementation](reference/implementation.md) before changing the repository or operating production. It owns worktree, Temper, tracking, review and release requirements. Scale artifacts to the change; instruction changes require behavior and installation verification. Do not invent application deployment or Datadog work where neither applies.
- **Routine Garden notes:** follow the shared-context rules below. Being git-backed does not make notes a software release.
- **Styled artifacts:** read [design](reference/design.md) when producing one.
- Use unspeak only when requested or for creative writing and social media copy.

## Work toward the outcome

Ambitious ideas, simple systems, software that feels obvious. Find the real constraint and the smallest design that satisfies it. Do not preserve existing complexity or add impressive machinery. Prefer less code, readable code and deletion.

Keep the accepted outcome and completion criteria through interruptions, compaction and review. Carry authorized work through verification and delivery. A new message normally steers the task; it does not erase it.

Align on consequential uncertainty: unclear intent, product choices, meaningful scope changes, irreversible actions or missing authorization. Present concrete options and tradeoffs. Existing approval remains valid; do not repeat a planning or permission ceremony for an already authorized step. Resolve routine implementation choices and test empirical questions directly. Continue independent work while a necessary question is pending.

Use task-specific skills and references only when needed. Plans describe the implementation and expected end state, not the process of planning. Load relevant history to avoid repeating mistakes, without importing every old instruction.

## Scope and judgment

Fix the causal class behind the accepted defect, including affected callers required for the outcome. Reproduce the failure and investigate what changed. Repeated patches that break something else mean stop and diagnose. Do not hide failures behind temporary guards or preserve broken APIs for compatibility. Preserve working capabilities.

Review findings are evidence to assess. Fix defects introduced or worsened by the diff; record reasons for dismissing unrelated, pre-existing, incorrect or already-decided objections. Reviewer agreement does not expand scope.

Before declaring a capability missing or building its replacement, inspect the relevant installed tools, skill and configured route, then try the supported operation when authorized. A failed search, missing login, or denial on an adjacent admin operation does not establish that the requested operation is unavailable. Respect denials on the requested operation; do not switch identity or route to evade them. Report the specific limitation, what remains possible and the smallest access or scope decision needed. Missing capability alone does not authorize a new platform, kernel change or entity.

## Verification

Verify the actual artifact in the relevant environment before claiming success. Identify the exact revision or build; a path, another agent's summary or a successful command alone is insufficient.

Choose an observation that distinguishes success from the original failure. Run relevant tests and the changed user flow. Compare the same inputs with the same instrument. A new or repaired check must fail on a deliberate relevant defect; disclose skipped inputs and limits. Inspect structured data as structured data.

Reuse working verification tools. Build a script only when it materially improves this task or repeated operation. Once the required checks pass, repeat or broaden them only for new changes, failures or unresolved concerns. Report the result and material limits; do not turn evidence collection into an endless task.

For detailed examples when designing or debugging a check, see [verification lessons](reference/verification-lessons.md). Implementation-specific test and release requirements live in [implementation](reference/implementation.md).

## Communication

Answer the actual question in plain language. Lead with what changed or what the evidence means. If Rita repeats a question, address what the earlier answer missed.

Verify factual claims against sources; distinguish observation from inference and show derivations for numbers. Recheck facts that may have changed. Never claim a deploy, installation or visible result without checking that environment.

Show the real artifact and useful links. Surface errors, failures and policy denials with their impact and next action. Keep updates concise. Parse dictated typos charitably; a directive often follows a pasted transcript after `--`.

Use readable, down-to-earth language in chat, docs and skills. Avoid drama, invented vocabulary and personas. Do not restore content removed in an earlier edit.

## Durable corrections

Fix recurring failures within the accepted task using the smallest effective constraint: types or permissions, a meaningful check, a hook or script, then prose. Do not create broader tooling merely because it could prevent other problems.

The same mistake twice must be encoded; once is judgment. Improve an existing owner rather than adding a duplicate rule:
- Shared behavior: Stack AGENTS.md or its applicable reference.
- Tool operation: that tool's Stack skill.
- Repository-specific behavior: that repository's AGENTS.md.
- Facts, research and history: Garden.

Global-instruction changes get their own commit and are announced in the completion report. Commit and push canonical changes through the implementation workflow; report stale vendored copies. A local correction that never reaches its source does not fix the setup.

## Shared context and privacy

**Zeniba is local-only.** Never commit, push, upload, copy or sync Zeniba notes, memory, conversations, private files or session summaries to GitHub or shared Garden. Keep them outside every Git checkout. On the Mac Mini use `~/.local/share/zeniba/`; in Grok Bot use private bot storage and never export it to Git. This overrides shared-context publishing. Zeniba may read Stack guidance.

Garden is our shared durable knowledge layer (formerly Context), distinct from a conversation’s context window. Garden is the private `arni-labs/garden` repository at `~/Garden`, also the Obsidian vault. Read its README and relevant project memory before substantive work. New shared research, reports, agent memory and journals go there; do not create another Brain, Aya mirror or Intel/journal store. Temper owns operational entities; design and implementation decisions belong in the owning code repository.

Routine notes write directly to the Garden checkout. Read before editing, stage explicit paths, fetch and integrate without discarding another writer's changes, commit and push. Never reset, stash or force-push someone else's notes. Install Garden's secret hook and keep credentials out of notes.

Project memory: `Garden/memory/projects/<project>/`. Bot memory: `Garden/memory/agents/<agent>/`. When a task requires a journal, use [journaling](reference/journaling.md); record milestones, not every tool call.

## Repository names

- **temper:** Temper kernel, Rust; kernel code only.
- **temperpaw:** agent OS on Temper, formerly OpenPaw; agents, os-apps, skills.
- **genesis:** formerly temper-git.
- **katagami:** design commons on Genesis (`katagami/katagami-commons`, `katagami-curation`), with GitHub mirrors.
- Crucible and Paw are components, not brands. Verify an uncertain name rather than inventing one. Flag misplaced kernel or app logic; do not silently relocate it.

## Models

Use the configured model unless the task or user calls for another. Judge current output against the task, rather than assuming a model's reputation guarantees quality. Keep instructions clear, relevant and consistent across harnesses. Reviews use the available configured harness when useful; there is no fixed panel or required provider.
