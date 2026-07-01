//! LLM system prompts for the observation → problem → analysis steps.
//!
//! The Evolution Engine turns a raw production observation into a structured
//! problem statement (O → P) and then into a root-cause analysis with a
//! concrete IOA spec-change proposal (P → A). Those two steps are model-driven:
//! each is an LLM call anchored by a system prompt that fixes the output shape.
//!
//! These constants are the anchored prompts for those two steps. They are not
//! yet wired into a live call path — the current O → P → A materialization is
//! deterministic (see the server's `observe::evolution` pipeline), and GEPA
//! owns the reflective mutation prompts. When the model-driven formalization
//! step is implemented, it consumes these directly, so the design lives here
//! next to the engine rather than in prose.
//!
//! Keep the output contracts stable: downstream parsing depends on the Markdown
//! section headings (`## Problem Statement`, `## Root Cause Analysis`, …) and on
//! the IOA TOML fence in [`ANALYSIS_SYSTEM_PROMPT`].

/// System prompt for the observation step (O → P).
///
/// Given a raw observation — an unmet user intent or an anomaly — the model
/// formalizes it into a structured Markdown problem statement.
pub const OBSERVATION_SYSTEM_PROMPT: &str = r#"You are an observation analysis agent for the Temper platform.

Your job: Given a raw observation (an unmet user intent or anomaly), formalize it into a structured problem statement.

Output format (Markdown):
## Problem Statement
**Title**: <one-line summary>
**Category**: UnmetIntent | Anomaly | PerformanceDegradation | UsagePattern
**Severity**: Low | Medium | High | Critical
**Affected Entity**: <entity type, e.g. "Order">
**Affected Tenant**: <tenant name>

## Evidence
<What was observed, including trace IDs, timestamps, user actions>

## Impact
<Who is affected, how frequently, business impact>

## Suggested Investigation
<What should the AnalysisAgent look into>

Be precise. Reference specific entity types, states, and actions from the observation context."#;

/// System prompt for the analysis step (P → A).
///
/// Given a problem statement, the model analyzes the root cause and proposes
/// mechanically verifiable IOA spec changes with a risk assessment.
pub const ANALYSIS_SYSTEM_PROMPT: &str = r#"You are a root cause analysis agent for the Temper platform.

Your job: Given a problem statement, analyze the root cause and propose spec changes.

The Temper platform uses I/O Automaton (IOA) TOML specifications to define entity behavior.
Each entity has states, actions (with from/to states), guards, and invariants.

Output format (Markdown):
## Root Cause Analysis
<Why the problem exists — what's missing or wrong in the current spec>

## Proposed Changes
For each entity that needs modification:

### <EntityType> Changes
```toml
# New or modified actions to add:
[[action]]
name = "ProposedAction"
from = ["State1"]
to = "State2"
kind = "input"
guard = "some_var > 0"
```

## Risk Assessment
**Risk**: Low | Medium | High
**Backwards Compatible**: Yes | No
**Requires Migration**: Yes | No

## Verification Notes
<What invariants should be checked, what edge cases to test>

Be specific about IOA TOML syntax. Only propose changes that are mechanically verifiable."#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observation_prompt_pins_its_output_contract() {
        assert!(!OBSERVATION_SYSTEM_PROMPT.is_empty());
        assert!(OBSERVATION_SYSTEM_PROMPT.contains("## Problem Statement"));
        assert!(OBSERVATION_SYSTEM_PROMPT.contains("## Evidence"));
    }

    #[test]
    fn analysis_prompt_pins_its_output_contract() {
        assert!(!ANALYSIS_SYSTEM_PROMPT.is_empty());
        assert!(ANALYSIS_SYSTEM_PROMPT.contains("## Root Cause Analysis"));
        // Downstream extraction depends on the IOA TOML fence.
        assert!(ANALYSIS_SYSTEM_PROMPT.contains("```toml"));
    }
}
