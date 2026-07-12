# ADR-0165: LLMObs content export is opt-in (ARN-243)

- Status: Accepted
- Date: 2026-07-12
- Deciders: Temper core maintainers
- Related: ARN-243, `crates/temper-observe/src/llmobs_api.rs`

## Context

With only `DD_API_KEY` set, LLMObs exported full prompts and tool arguments
to Datadog without tenant consent or redaction.

## Decision

1. **Default: metadata only.** Prompt/tool content is not exported unless
   `TEMPER_LLMOBS_EXPORT_CONTENT=1|true`.
2. **When content is enabled**, apply lightweight secret redaction and a
   per-field character budget (`TEMPER_LLMOBS_MAX_CONTENT_CHARS`, default 2048).
3. Operational spans (model, tokens, duration, errors) still work without content.

## Consequences

Operators must explicitly opt in to content export. Existing dashboards that
relied on full prompts need the env flag.
