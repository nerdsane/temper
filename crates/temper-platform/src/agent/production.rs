//! Production chat agent for end users.
//!
//! Handles user requests within the deployed application by dispatching
//! entity actions via the OData API and tracking unmet intents for the
//! evolution engine.
//!
//! Emits a `temper.prod_agent.turn` OTEL span per conversation turn.
//! When an [`UnmetIntent`] is captured, an `unmet_intent` span event links
//! back to the originating production trace for the Evolution Engine.

use chrono::Utc;
use opentelemetry::global;
use opentelemetry::trace::{Span, Tracer};
use opentelemetry::KeyValue;

use crate::agent::claude::{ClaudeClient, Message};
use crate::evolution::UnmetIntent;
use crate::protocol::WsMessage;
use crate::state::PlatformState;

/// Production chat agent.
///
/// Operates within the deployed specs, dispatching actions to entity actors.
/// When the system can't handle a user request, it captures an [`UnmetIntent`]
/// for the evolution pipeline.
pub struct ProductionAgent {
    /// Platform state for dispatching actions.
    state: PlatformState,
    /// Claude client for LLM-powered responses.
    claude: Option<ClaudeClient>,
    /// Conversation history.
    conversation: Vec<Message>,
    /// System prompt generated from OData metadata.
    system_prompt: String,
    /// Conversation turn counter (1-based).
    turn_number: u32,
}

impl ProductionAgent {
    /// Create a new production agent from platform state.
    ///
    /// Dynamically generates the system prompt from the registered entity specs.
    pub fn new(state: PlatformState) -> Self {
        let system_prompt = build_system_prompt(&state);
        let claude = state.api_key.clone().map(ClaudeClient::new);

        Self {
            state,
            claude,
            conversation: Vec::new(),
            system_prompt,
            turn_number: 0,
        }
    }

    /// Current conversation turn number (1-based, incremented per message).
    pub fn turn_number(&self) -> u32 {
        self.turn_number
    }

    /// Handle a user message and return response messages.
    ///
    /// Emits a `temper.prod_agent.turn` OTEL span per call. When an
    /// [`UnmetIntent`] is captured, an `unmet_intent` event is added to the span.
    pub async fn handle_message(&mut self, user_msg: &str) -> Vec<WsMessage> {
        self.turn_number += 1;
        self.conversation.push(Message::user(user_msg));

        let tracer = global::tracer("temper");
        let truncated_intent: String = user_msg.chars().take(200).collect();
        let mut span = tracer
            .span_builder("temper.prod_agent.turn")
            .with_attributes(vec![
                KeyValue::new("temper.turn", self.turn_number as i64),
                KeyValue::new("temper.user_intent", truncated_intent),
            ])
            .start(&tracer);

        let result = if let Some(ref claude) = self.claude {
            match claude.chat(&self.conversation, &self.system_prompt).await {
                Ok(response) => {
                    self.conversation.push(Message::assistant(&response));

                    // Check for tool call patterns in the response
                    let tool_result = self.try_execute_tool(&response).await;
                    match tool_result {
                        Some(Ok(result)) => {
                            vec![WsMessage::AgentResponse {
                                content: result,
                                done: true,
                            }]
                        }
                        Some(Err(unmet)) => {
                            span.add_event(
                                "unmet_intent",
                                vec![
                                    KeyValue::new(
                                        "temper.user_intent",
                                        unmet.user_intent.clone(),
                                    ),
                                    KeyValue::new(
                                        "temper.attempted_tool",
                                        unmet.attempted_tool.clone(),
                                    ),
                                    KeyValue::new("temper.tenant", unmet.tenant.clone()),
                                ],
                            );
                            crate::evolution::UnmetIntentCollector::collect(&unmet, &self.state);
                            vec![WsMessage::AgentResponse {
                                content: format!(
                                    "I wasn't able to complete that action. \
                                     The request has been logged for the development team. ({})",
                                    unmet.user_intent,
                                ),
                                done: true,
                            }]
                        }
                        None => {
                            vec![WsMessage::AgentResponse {
                                content: response,
                                done: true,
                            }]
                        }
                    }
                }
                Err(err) => {
                    vec![WsMessage::Error {
                        message: format!("AI agent error: {err}"),
                    }]
                }
            }
        } else {
            self.handle_template_message(user_msg).await
        };

        span.end();
        result
    }

    /// Template-based response when no Claude API key is available.
    async fn handle_template_message(&mut self, user_msg: &str) -> Vec<WsMessage> {
        let lower = user_msg.trim().to_lowercase();

        let registry = self.state.registry.read().unwrap();
        let tenants = registry.tenant_ids();

        if tenants.is_empty() {
            return vec![WsMessage::AgentResponse {
                content: "No applications are deployed yet. Please set up your application \
                         through the developer chat first."
                    .into(),
                done: true,
            }];
        }

        // Build capabilities summary
        let mut capabilities = String::from("Available operations:\n\n");
        for tenant_id in &tenants {
            let entity_types = registry.entity_types(tenant_id);
            for et in &entity_types {
                if let Some(spec) = registry.get_spec(tenant_id, et) {
                    capabilities.push_str(&format!("**{}**:\n", et));
                    for action in &spec.automaton.actions {
                        let hint = action.hint.as_deref().unwrap_or("No description");
                        capabilities.push_str(&format!("  - {} — {}\n", action.name, hint));
                    }
                    capabilities.push('\n');
                }
            }
        }
        drop(registry);

        if lower.contains("list") || lower.contains("show") || lower.contains("help") {
            return vec![WsMessage::AgentResponse {
                content: capabilities,
                done: true,
            }];
        }

        vec![WsMessage::AgentResponse {
            content: format!(
                "I understand you want to: \"{user_msg}\"\n\n{capabilities}\n\
                 Try asking me to perform one of these operations.",
            ),
            done: true,
        }]
    }

    /// Try to execute a tool call embedded in Claude's response.
    async fn try_execute_tool(&self, response: &str) -> Option<Result<String, UnmetIntent>> {
        if !response.contains("EXECUTE:") {
            return None;
        }

        let action_str = response.split("EXECUTE:").nth(1)?;
        let parts: Vec<&str> = action_str.trim().splitn(3, ' ').collect();
        if parts.len() < 3 {
            return None;
        }

        let entity_type = parts[0];
        let entity_id = parts[1];
        let action = parts[2].trim();

        let result = self
            .state
            .server
            .dispatch_action(entity_type, entity_id, action, serde_json::json!({}))
            .await;

        match result {
            Ok(resp) if resp.success => Some(Ok(format!(
                "Done! {} is now in state: {}",
                entity_type, resp.state.status,
            ))),
            _ => Some(Err(UnmetIntent {
                user_intent: action.to_string(),
                attempted_tool: format!("{entity_type}.{action}"),
                tenant: "default".into(),
                trace_id: uuid::Uuid::now_v7().to_string(),
                timestamp: Utc::now(),
            })),
        }
    }
}

/// Build the system prompt from the platform's registered specs.
fn build_system_prompt(state: &PlatformState) -> String {
    let mut prompt = String::from(
        "You are a helpful assistant operating a Temper application. \
         You help users interact with the system by performing actions on entities.\n\n",
    );

    let registry = state.registry.read().unwrap();
    let tenants = registry.tenant_ids();

    if tenants.is_empty() {
        prompt.push_str("No entities are currently deployed.\n");
        return prompt;
    }

    prompt.push_str("Available entities and actions:\n\n");

    for tenant_id in &tenants {
        let entity_types = registry.entity_types(tenant_id);
        for et in &entity_types {
            if let Some(spec) = registry.get_spec(tenant_id, et) {
                prompt.push_str(&format!("Entity: {et}\n"));
                prompt.push_str(&format!(
                    "  States: {}\n",
                    spec.automaton.automaton.states.join(", "),
                ));
                prompt.push_str(&format!("  Initial: {}\n", spec.automaton.automaton.initial));

                for action in &spec.automaton.actions {
                    prompt.push_str(&format!("  Action: {}\n", action.name));
                    if !action.from.is_empty() {
                        prompt.push_str(&format!("    From: {}\n", action.from.join(", ")));
                    }
                    if let Some(ref to) = action.to {
                        prompt.push_str(&format!("    To: {to}\n"));
                    }
                    if let Some(ref hint) = action.hint {
                        prompt.push_str(&format!("    Hint: {hint}\n"));
                    }
                }
                prompt.push('\n');
            }
        }
    }

    prompt.push_str(
        "\nTo execute an action, include: EXECUTE: EntityType EntityId ActionName\n",
    );

    prompt
}

#[cfg(test)]
mod tests {
    use super::*;
    use temper_server::registry::SpecRegistry;

    #[test]
    fn test_production_agent_creation() {
        let state = PlatformState::new_production(SpecRegistry::new(), None);
        let agent = ProductionAgent::new(state);
        assert!(agent.conversation.is_empty());
        assert!(agent.claude.is_none());
        assert_eq!(agent.turn_number(), 0);
    }

    #[test]
    fn test_build_system_prompt_empty() {
        let state = PlatformState::new_production(SpecRegistry::new(), None);
        let prompt = build_system_prompt(&state);
        assert!(prompt.contains("No entities are currently deployed"));
    }

    #[test]
    fn test_build_system_prompt_with_specs() {
        use temper_spec::csdl::parse_csdl;

        let csdl_xml = include_str!("../../../../test-fixtures/specs/model.csdl.xml");
        let order_ioa = include_str!("../../../../test-fixtures/specs/order.ioa.toml");
        let csdl = parse_csdl(csdl_xml).unwrap();

        let mut registry = SpecRegistry::new();
        registry.register_tenant("ecommerce", csdl, csdl_xml.to_string(), &[("Order", order_ioa)]);

        let state = PlatformState::new_production(registry, None);
        let prompt = build_system_prompt(&state);

        assert!(prompt.contains("Entity: Order"));
        assert!(prompt.contains("States:"));
        assert!(prompt.contains("Action:"));
    }

    #[tokio::test]
    async fn test_production_agent_no_deployments() {
        let state = PlatformState::new_production(SpecRegistry::new(), None);
        let mut agent = ProductionAgent::new(state);
        let responses = agent.handle_message("hello").await;

        assert!(!responses.is_empty());
        let has_msg = responses.iter().any(|m| {
            matches!(m, WsMessage::AgentResponse { content, .. } if content.contains("No applications"))
        });
        assert!(has_msg);
    }

    #[tokio::test]
    async fn test_production_agent_help() {
        use temper_spec::csdl::parse_csdl;

        let csdl_xml = include_str!("../../../../test-fixtures/specs/model.csdl.xml");
        let order_ioa = include_str!("../../../../test-fixtures/specs/order.ioa.toml");
        let csdl = parse_csdl(csdl_xml).unwrap();

        let mut registry = SpecRegistry::new();
        registry.register_tenant("ecommerce", csdl, csdl_xml.to_string(), &[("Order", order_ioa)]);

        let state = PlatformState::new_production(registry, None);
        let mut agent = ProductionAgent::new(state);
        let responses = agent.handle_message("help").await;

        let has_ops = responses.iter().any(|m| {
            matches!(m, WsMessage::AgentResponse { content, .. } if content.contains("Order"))
        });
        assert!(has_ops, "Should list available operations");
    }

    #[tokio::test]
    async fn test_production_agent_increments_turn() {
        let state = PlatformState::new_production(SpecRegistry::new(), None);
        let mut agent = ProductionAgent::new(state);
        assert_eq!(agent.turn_number(), 0);

        agent.handle_message("hello").await;
        assert_eq!(agent.turn_number(), 1);

        agent.handle_message("help").await;
        assert_eq!(agent.turn_number(), 2);

        agent.handle_message("list operations").await;
        assert_eq!(agent.turn_number(), 3);
    }
}
