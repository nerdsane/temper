# ADR-0081: LLMObs Agent Workflow Hierarchy

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

The agent root uses the active APM parent span id when one exists, so the LLMObs tree and APM trace remain joinable without inventing a second correlation model. The workflow span id is deterministically derived from the trace id and LLM span id. LLM messages, token metrics, model provider, and model name stay on the LLM span to avoid repeating large prompt/response payloads on parent spans.

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

### Negative

- The agent root duration is an envelope around the observed LLM call, not the complete wall-clock lifetime of a long-lived session.
- Multiple LLM turns in one session produce separate workflow spans and may repeat the agent root if Datadog receives them in separate payloads.
- Parent agent/workflow IO values intentionally duplicate a short summary of the LLM child input/output; full message arrays remain only on the LLM span.

## Non-Goals

- Do not mirror every Temper dispatch span into LLMObs.
- Do not replace APM traces, logs, metrics, or Postgres DBM.
- Do not add a new orchestration layer outside entity actions and WASM integrations.

## Rollback Policy

Remove the agent/workflow spans from the direct LLMObs payload builder and make tool spans fall back to the previous LLM span parent. APM and wide-event metrics continue to work independently.
