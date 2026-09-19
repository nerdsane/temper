# Working with agents

## Start with the result

Describe the outcome, important constraints, available evidence and what completion means. Add authority when it is not already clear. You do not need to prescribe every step.

Example:

> Fix the gallery loading delay after sign-in. Preserve visibility rules and the current design. Reproduce it, fix the cause, verify the changed flow and carry the change through the normal release. Resolve routine implementation choices yourself. Bring me product tradeoffs or a required scope change with concrete options.

For exploration, name the decision you want to make:

> Investigate why gallery loading is slow. Show the evidence and the smallest options. Do not implement yet.

## Know where work happens

A local session works in a local isolated worktree. A session inside a Tensorlake computer works on that computer. Temper tracking does not require a remote worktree. A task may need a specific remote operation; the agent should explain that dependency without moving all development remotely.

When moving a task, preserve its current work and verify the destination before retiring the source runtime.

## Steer the result

A correction should name what is wrong and the intended behavior. The agent should preserve the rest of the objective and previous approvals.

Useful interventions:
- “That adds scope. Finish the original outcome; record this separately.”
- “Show the observation that distinguishes the fix from the old behavior.”
- “You already have approval for that step. Continue.”
- “This is a product decision. Give me the concrete tradeoff.”

When work stalls, ask what prevents the next observable result. A failed credential or missing runtime requires an operational fix; longer prompting does not repair it.

## Supervise where judgment matters

Ask for comparison or a prototype when a choice is consequential and evidence is missing. Routine work should proceed without an interview. Review the actual result and key tradeoffs, rather than rewarding the amount of process reported.

A good checkpoint contains what works, evidence, and the remaining uncertainty. Verification ends when the relevant checks pass, unless a new change or failure justifies more work.

## See exactly what agents review

Start with [the shared review contract](../REVIEW.md): the accepted outcome, concrete defects caused by the change, and independent testing of meaningful features when useful. Review is advice, not another approval process. The full criteria are short enough to read directly.

Each repository adds only checks relevant to its software, applied when the change touches them:

| Repository | Additional focus | Current brief |
| --- | --- | --- |
| Temper | Simulation determinism, state invariants, tenant authorization, replay/recovery | [REVIEW.md](https://github.com/nerdsane/temper/blob/main/REVIEW.md) |
| TemperPaw | App specs/policies/integrations, OData flows, installed app behavior | [REVIEW.md](https://github.com/nerdsane/temperpaw/blob/main/REVIEW.md) |
| Foundry | Session continuity, streaming, organization isolation, real UI | [REVIEW.md](https://github.com/arni-labs/foundry/blob/main/REVIEW.md) |
| Genesis | Real git round trips, object/ref integrity, storage durability | [REVIEW.md](https://github.com/arni-labs/genesis/blob/main/REVIEW.md) |
| Katagami | Gallery/contribution flows, images/layout, permissions and app contracts | [REVIEW.md](https://github.com/arni-labs/katagami/blob/master/REVIEW.md) |
| Deep Sci-Fi | Agent API/story flows, authorization, database migrations | [REVIEW.md](https://github.com/arni-labs/deep-sci-fi/blob/main/REVIEW.md) |

This table is a navigation aid; the linked briefs own the details. For your own review, read the PR's behavior change and verification result, then try the changed flow. A missing panel or paperwork artifact does not mean a missing product check.

## Use memory without accumulating rules

Record stable environment facts and decisions in Garden. Keep operating instructions at their canonical owner. When a recurring failure needs a durable fix, improve a check, tool or existing instruction; avoid adding the same rule to every prompt and skill.

Start a fresh session when changing objectives or when obsolete conversation is interfering. Carry forward the current objective, approvals, artifact links and unresolved issues. Compaction or a new session should not restart completed work.

## Why this setup changed

OpenAI's [Astra prompt and skill guidance](https://developers.openai.com/blog/rethinking-skills-and-prompts-for-gpt-6-astra) recommends clearer triggers, progressive disclosure and less accumulated prescription. The [model guide](https://developers.openai.com/api/docs/guides/latest-model) provides current model-specific guidance.

poteto's [pstack part 1](https://x.com/poteto/status/2094457600259842065) emphasizes runnable controls, feature maps and usable environments. [Part 2](https://x.com/poteto/status/2097732320606507506) grounds supervision in relevant history, caller experience, experiments and conditional playbooks. Arni mode retains those ideas while Stack owns the shared release rules.

These are starting principles. Judge the setup by successful outcomes, unnecessary interventions, repeated work and honest verification on your tasks.
