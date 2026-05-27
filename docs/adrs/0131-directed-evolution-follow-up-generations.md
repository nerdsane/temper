# ADR-0131: Directed Evolution Follow-Up Generations

- Status: Accepted
- Date: 2026-05-27
- Deciders: Temper core maintainers
- Related:
  - ADR-0128: Directed Evolution Policy-Gated Repair Autostart
  - ADR-0130: Directed Evolution Worker Stage Cleanup
  - `os-apps/directed-evolution/wasm/work_item_result_router`
  - `os-apps/directed-evolution/specs/work_item.ioa.toml`

## Context

The first live repair run after repair-aware variant lanes reached a real
terminal failure: every variant was hot-loaded, evaluated by simulated-user and
reviewer brains, and eliminated. The evidence was useful. It showed that the
variants exposed CSDL navigation metadata, but runtime navigation still failed
because submitted answers persisted `question_id` while the relationship expected
`QuestionId`.

Stopping the episode immediately wastes that learning. Directed Evolution needs
generational pressure: a failed generation should feed its elimination evidence
into a bounded follow-up generation before the episode fails.

## Decision

When all variants in a generation are eliminated and the episode is still
running, the work-item result router may create one follow-up generation up to a
configured `max_generation_count`.

The follow-up generation:

- records the previous generation as failed;
- stays within the same episode, Adaptation Goal, Viability Constraints, and
  Selection Pressure;
- queues fresh variant-generator work items;
- injects eliminated variant summaries and stage evidence into the new prompts;
- tells variant generators not to repeat a metadata-only mutation family when
  runtime evidence showed the state field still did not resolve.

If the maximum generation count has been reached, the existing behavior remains:
the generation and episode fail honestly.

## Rollout Plan

1. **Phase 0 (Immediate)** - Set `max_generation_count = 2` on the
   work-item-result router trigger and ship follow-up generation creation.
2. **Phase 1 (Live proof)** - Hot-load Directed Evolution and run another repair
   episode. Generation 1 should fail with evidence and generation 2 should
   produce sharper variants.
3. **Phase 2 (Policy/UI)** - Surface generation retry count and evidence-fed
   prompts in Mission Control.

## Readiness Gates

- A no-survivor generation creates a second generation instead of failing the
  episode immediately.
- Follow-up variant prompts include the previous generation's elimination
  evidence.
- The second failed generation still fails the episode rather than looping.

## Consequences

### Positive

- The engine behaves more like directed evolution: failed variants become useful
  pressure for the next generation.
- The human can see the organism learn from elimination evidence rather than
  repeatedly generating the same family of repair.

### Negative

- Episodes can run longer and consume more Codex work.
- The V1 cap is static trigger config rather than a negotiated episode-level
  budget.

### Risks

- If the evidence summary is noisy, follow-up variants may overfit. The
  mitigation is that Viability Constraints and evaluator stages remain unchanged
  across generations.

### DST Compliance

- This change is inside a WASM OS-app integration and app trigger config. It uses
  deterministic entity scans and bounded generation counts; it does not add
  wall-clock time, random IDs outside Temper entity creation, or new threads.

## Non-Goals

- Infinite or open-ended evolution loops.
- Human-negotiated generation budgets.
- Changing selection, promotion, or evaluator authority.

## Alternatives Considered

1. **Fail immediately after one no-survivor generation** - Rejected because live
   evidence showed the engine had learned enough to make a better second attempt.
2. **Start a separate new episode** - Rejected because the evidence belongs to
   the same Adaptation Goal and should remain inside the episode genealogy.
3. **Let the selector choose among eliminated variants** - Rejected because
   eliminated variants violated the Viability Constraints.

## Rollback Policy

Set `max_generation_count = 1` or revert the router change. Episodes will again
fail after the first no-survivor generation.
