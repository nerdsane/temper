# ADR-0171: LLMObs Agent Workflow Hierarchy

- Status: Accepted
- Date: 2026-05-12
- Deciders: Temper core maintainers
- Related:
  - `crates/temper-observe/src/llmobs_api.rs`
  - `crates/temper-observe/src/llmobs_format.rs`
  - `crates/temper-server/src/state/dispatch/wasm.rs`

## Context

Temper emits detailed APM traces for entity dispatch and WASM integrations, and LLM integrations emit Datadog LLM Observability spans through the direct intake API. The direct LLM span had the right TemperPaw service identity and APM trace correlation, but it could appear in LLMObs without a visible root. Datadog then showed a one-span LLM trace with a missing parent warning even though the APM trace had the full dispatch tree.

For agent debugging, LLMObs needs a human-readable tree, not a copy of every internal database, OData, and actor span. The useful shape is the agent/session run, the workflow turn that invoked the model, the content-bearing LLM call, and tool spans attached to that workflow.

## Decision

Temper's direct LLMObs payloads emit a compact hierarchy for every LLM integration:

1. An `agent` root span named `temperpaw.agent.session`.
2. A `workflow` child span named from the Temper entity action, such as `Session.ContextReady`.
3. The existing content-bearing `llm` span, nested under the workflow span.

The agent root span id is stable for the logical TemperPaw session. Temper
derives it from the active trace id and session id on the first LLM turn, then
persists `llmobs_agent_span_id` and `llmobs_agent_start_ns` through callback
state so later LLM turns and tool turns attach to the same root instead of
creating one root per provider call. The workflow span id remains
deterministically derived from the trace id and LLM span id, giving each model
turn its own workflow child under the session root. LLM messages, token metrics,
model provider, and model name stay on the LLM span to avoid repeating large
prompt/response payloads on parent spans.

The agent and workflow spans carry only compact `input.value` / `output.value` summaries derived from the LLM call. This keeps Datadog's agent-loop and Trace Explorer views useful for humans and agents while avoiding duplicated chat `messages` arrays on every parent span.

Tool spans prefer the persisted LLMObs workflow span id as their parent. This keeps tool execution as a sibling of the LLM call under the same workflow turn instead of hiding tools under the model span or leaving them with a missing parent.

APM remains the authoritative low-level trace for detailed dispatch, database, OData, actor, and WASM timing. LLMObs remains the semantic agent trace for humans and agents inspecting model/tool behavior and bottlenecks.

## Consequences

### Positive

- LLMObs traces have visible roots and no missing-parent warning for ordinary TemperPaw LLM calls.
- Humans see a concise chronological tree rather than hundreds of internal spans.
- Agent-loop tooling can narrate the root and workflow spans because those spans have compact IO values.
- Agents can query by `session_id`, `ml_app`, model, provider, and span kind to reconstruct the model/tool flow.
- APM trace correlation is preserved for deeper runtime debugging.
- Multi-turn sessions with tools remain one LLMObs session tree instead of a
  forest of repeated agent roots; Datadog's trace tree can show the first LLM
  call, tool execution, and final LLM call chronologically under the same agent
  span.

### Negative

- The agent root duration is still an observability envelope based on observed
  LLM turns, not a perfect wall-clock measurement of all possible non-LLM
  session work.
- Re-submitting the same agent span id on later turns relies on Datadog trace
  assembly de-duplicating by trace id/span id. Live TemperPaw verification must
  continue checking multi-turn tool sessions because this behavior is part of
  the Datadog intake contract we depend on.
- Parent agent/workflow IO values intentionally duplicate a short summary of the LLM child input/output; full message arrays remain only on the LLM span.

## Non-Goals

- Do not mirror every Temper dispatch span into LLMObs.
- Do not replace APM traces, logs, metrics, or Postgres DBM.
- Do not add a new orchestration layer outside entity actions and WASM integrations.

## Rollback Policy

Remove the agent/workflow spans from the direct LLMObs payload builder and make tool spans fall back to the previous LLM span parent. APM and wide-event metrics continue to work independently.
