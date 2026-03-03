//! Agentic evolution: Claude-powered O→P and P→A transformations.
//!
//! - [`ObservationAgent`]: Formalizes an O-Record (raw observation) into a
//!   P-Record (Lamport-style problem statement).
//! - [`AnalysisAgent`]: Analyzes a P-Record and proposes an A-Record with
//!   root cause analysis and spec diff suggestions.
//!
//! Both agents use the [`ClaudeClient`] with role-specific system prompts.
//! They are invoked by the evolution pipeline when high-priority insights
//! are detected — the developer still holds the approval gate (D-Record).

use crate::agent::claude::{ChatClient, ClaudeClient, ClaudeError, Message};

/// Formalizes observations into problem statements (O→P).
///
/// Given a raw observation (unmet intent, anomaly, trajectory pattern),
/// the ObservationAgent uses Claude to produce a structured problem
/// statement following the OPADI record format.
///
/// Generic over `C: ChatClient` to support both real API calls and mock
/// clients in tests.
pub struct ObservationAgent<C: ChatClient = ClaudeClient> {
    client: C,
}

const OBSERVATION_SYSTEM_PROMPT: &str = r#"You are an observation analysis agent for the Temper platform.

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

impl ObservationAgent {
    /// Create a new ObservationAgent with the real Claude API client.
    pub fn new(api_key: String) -> Self {
        Self {
            client: ClaudeClient::new(api_key),
        }
    }
}

impl<C: ChatClient> ObservationAgent<C> {
    /// Create a new ObservationAgent with a custom chat client.
    pub fn with_client(client: C) -> Self {
        Self { client }
    }

    /// Formalize an observation into a problem statement.
    ///
    /// `observation_context` is a JSON string with fields like:
    /// - `user_intent`: what the user tried to do
    /// - `attempted_tool`: the action that failed
    /// - `tenant`: which tenant
    /// - `trace_id`: for correlation
    pub async fn formalize(&self, observation_context: &str) -> Result<String, ClaudeError> {
        let messages = vec![Message::user(format!(
            "Formalize this observation into a problem statement:\n\n{}",
            observation_context,
        ))];

        self.client.chat(&messages, OBSERVATION_SYSTEM_PROMPT).await
    }
}

/// Analyzes problems and proposes spec changes (P→A).
///
/// Given a formalized problem statement (P-Record), the AnalysisAgent
/// uses Claude to perform root cause analysis and propose IOA spec
/// modifications that would resolve the issue.
///
/// Generic over `C: ChatClient` to support both real API calls and mock
/// clients in tests.
pub struct AnalysisAgent<C: ChatClient = ClaudeClient> {
    client: C,
}

const ANALYSIS_SYSTEM_PROMPT: &str = r#"You are a root cause analysis agent for the Temper platform.

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

impl AnalysisAgent {
    /// Create a new AnalysisAgent with the real Claude API client.
    pub fn new(api_key: String) -> Self {
        Self {
            client: ClaudeClient::new(api_key),
        }
    }
}

impl<C: ChatClient> AnalysisAgent<C> {
    /// Create a new AnalysisAgent with a custom chat client.
    pub fn with_client(client: C) -> Self {
        Self { client }
    }

    /// Analyze a problem and propose spec changes.
    ///
    /// `problem_statement` is the P-Record output from ObservationAgent.
    /// `current_specs` is a map of entity_type → IOA TOML source for context.
    pub async fn analyze(
        &self,
        problem_statement: &str,
        current_specs: &[(String, String)],
    ) -> Result<String, ClaudeError> {
        let mut prompt = format!(
            "Analyze this problem and propose spec changes:\n\n{}\n\n",
            problem_statement,
        );

        if !current_specs.is_empty() {
            prompt.push_str("## Current Specs\n\n");
            for (entity_type, ioa_source) in current_specs {
                prompt.push_str(&format!(
                    "### {entity_type}\n```toml\n{ioa_source}\n```\n\n"
                ));
            }
        }

        let messages = vec![Message::user(prompt)];
        self.client.chat(&messages, ANALYSIS_SYSTEM_PROMPT).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::claude::MockClaudeClient;

    #[test]
    fn test_observation_agent_creation() {
        let _agent = ObservationAgent::new("test-key".to_string());
    }

    #[test]
    fn test_analysis_agent_creation() {
        let _agent = AnalysisAgent::new("test-key".to_string());
    }

    #[test]
    fn test_system_prompts_are_non_empty() {
        assert!(!OBSERVATION_SYSTEM_PROMPT.is_empty());
        assert!(!ANALYSIS_SYSTEM_PROMPT.is_empty());
        assert!(OBSERVATION_SYSTEM_PROMPT.contains("Problem Statement"));
        assert!(ANALYSIS_SYSTEM_PROMPT.contains("Root Cause"));
    }

    #[tokio::test]
    async fn test_observation_agent_with_mock() {
        let mock = MockClaudeClient::fixed(
            "## Problem Statement\n**Title**: High p99 latency\n**Category**: Performance",
        );
        let agent = ObservationAgent::with_client(mock);
        let result = agent
            .formalize(r#"{"user_intent": "place order", "trace_id": "abc-123"}"#)
            .await;
        let text = result.expect("mock should return Ok");
        assert!(text.contains("Problem Statement"));
        assert!(text.contains("High p99 latency"));
    }

    #[tokio::test]
    async fn test_analysis_agent_with_mock() {
        let mock = MockClaudeClient::fixed(
            "## Root Cause Analysis\nMissing guard on SubmitOrder action.\n\n## Risk Assessment\n**Risk**: Low",
        );
        let agent = AnalysisAgent::with_client(mock);
        let specs = vec![("Order".to_string(), "# order spec".to_string())];
        let result = agent
            .analyze("High p99 latency in Order entity", &specs)
            .await;
        let text = result.expect("mock should return Ok");
        assert!(text.contains("Root Cause"));
        assert!(text.contains("Missing guard"));
    }
}
