//! Developer interview agent.
//!
//! Orchestrates the structured interview flow that discovers entities, states,
//! actions, guards, and invariants from a developer conversation. Supports both
//! LLM-powered (Claude) and template-based (offline) modes.
//!
//! Emits a `temper.dev_agent.turn` OTEL span per conversation turn with
//! turn number, interview phase, and user intent attributes.

use opentelemetry::global;
use opentelemetry::trace::{Span, Tracer};
use opentelemetry::KeyValue;

use crate::agent::claude::{ClaudeClient, Message};
use crate::interview::entity_collector::*;
use crate::interview::spec_generator;
use crate::interview::InterviewPhase;
use crate::protocol::{SpecType, WsMessage};

/// Developer interview agent that collects entity models from conversation.
pub struct DeveloperAgent {
    /// Current interview phase.
    phase: InterviewPhase,
    /// Completed entity models.
    entities: Vec<EntityModel>,
    /// Entity model currently being built.
    current_entity: Option<EntityModel>,
    /// Claude client for LLM-powered mode.
    claude: Option<ClaudeClient>,
    /// Conversation history.
    conversation: Vec<Message>,
    /// Tenant name for the project.
    tenant_name: String,
    /// Conversation turn counter (1-based).
    turn_number: u32,
}

impl DeveloperAgent {
    /// Create a new developer agent.
    ///
    /// If `api_key` is provided, uses Claude for responses. Otherwise falls
    /// back to template-based responses.
    pub fn new(tenant_name: String, api_key: Option<String>) -> Self {
        let claude = api_key.map(ClaudeClient::new);
        Self {
            phase: InterviewPhase::Welcome,
            entities: Vec::new(),
            current_entity: None,
            claude,
            conversation: Vec::new(),
            tenant_name,
            turn_number: 0,
        }
    }

    /// Current interview phase.
    pub fn phase(&self) -> &InterviewPhase {
        &self.phase
    }

    /// Completed entity models.
    pub fn entities(&self) -> &[EntityModel] {
        &self.entities
    }

    /// Current conversation turn number (1-based, incremented per message).
    pub fn turn_number(&self) -> u32 {
        self.turn_number
    }

    /// Handle a user message and return response messages.
    ///
    /// In template mode (no API key), uses simple pattern matching to
    /// drive the interview. In LLM mode, delegates to Claude.
    ///
    /// Emits a `temper.dev_agent.turn` OTEL span per call.
    pub async fn handle_message(&mut self, user_msg: &str) -> Vec<WsMessage> {
        self.turn_number += 1;
        self.conversation.push(Message::user(user_msg));

        let tracer = global::tracer("temper");
        let truncated_intent: String = user_msg.chars().take(200).collect();
        let mut span = tracer
            .span_builder("temper.dev_agent.turn")
            .with_attributes(vec![
                KeyValue::new("temper.turn", self.turn_number as i64),
                KeyValue::new("temper.phase", self.phase.display_name().to_string()),
                KeyValue::new("temper.user_intent", truncated_intent),
                KeyValue::new("temper.tenant", self.tenant_name.clone()),
            ])
            .start(&tracer);

        let result = if self.claude.is_some() {
            self.handle_llm_message(user_msg).await
        } else {
            self.handle_template_message(user_msg)
        };

        span.set_attribute(KeyValue::new(
            "temper.phase_after",
            self.phase.display_name().to_string(),
        ));
        span.end();

        result
    }

    /// Handle a message using the Claude LLM.
    async fn handle_llm_message(&mut self, _user_msg: &str) -> Vec<WsMessage> {
        let system = self.system_prompt();
        let claude = self.claude.as_ref().unwrap();

        match claude.chat(&self.conversation, &system).await {
            Ok(response) => {
                self.conversation.push(Message::assistant(&response));
                vec![WsMessage::AgentResponse {
                    content: response,
                    done: true,
                }]
            }
            Err(err) => {
                vec![WsMessage::Error {
                    message: format!("Claude API error: {err}"),
                }]
            }
        }
    }

    /// Handle a message using template-based responses.
    fn handle_template_message(&mut self, user_msg: &str) -> Vec<WsMessage> {
        match &self.phase {
            InterviewPhase::Welcome => self.handle_welcome(user_msg),
            InterviewPhase::EntityDiscovery => self.handle_entity_discovery(user_msg),
            InterviewPhase::StateDiscovery => self.handle_state_discovery(user_msg),
            InterviewPhase::ActionDiscovery => self.handle_action_discovery(user_msg),
            InterviewPhase::GuardDiscovery => self.handle_guard_discovery(user_msg),
            InterviewPhase::InvariantDiscovery => self.handle_invariant_discovery(user_msg),
            InterviewPhase::SpecReview => self.handle_spec_review(user_msg),
            InterviewPhase::Verifying | InterviewPhase::Deployed => {
                vec![WsMessage::AgentResponse {
                    content: "Your application is deployed! Use the production chat to interact with it.".to_string(),
                    done: true,
                }]
            }
        }
    }

    fn handle_welcome(&mut self, _user_msg: &str) -> Vec<WsMessage> {
        let mut msgs = vec![WsMessage::AgentResponse {
            content: format!(
                "Welcome to Temper! I'll help you build your application for '{}'.\n\n\
                 Let's start by identifying the main entities in your application.\n\
                 What are the key things (entities) your application needs to manage?\n\n\
                 For example: Order, Task, Invoice, Ticket, etc.\n\
                 List them separated by commas.",
                self.tenant_name
            ),
            done: true,
        }];
        msgs.extend(self.advance_phase());
        msgs
    }

    fn handle_entity_discovery(&mut self, user_msg: &str) -> Vec<WsMessage> {
        // Parse entity names from comma-separated input
        let names: Vec<String> = user_msg
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        if names.is_empty() {
            return vec![WsMessage::AgentResponse {
                content: "Please provide at least one entity name (e.g., 'Order, Task').".to_string(),
                done: true,
            }];
        }

        // Create entity stubs
        for name in &names {
            self.entities.push(EntityModel {
                name: name.clone(),
                ..Default::default()
            });
        }

        // Start working on the first entity
        self.current_entity = Some(self.entities[0].clone());

        let entity_list = names.join(", ");
        let first = &names[0];
        let mut msgs = vec![WsMessage::AgentResponse {
            content: format!(
                "Great! I've noted these entities: {entity_list}.\n\n\
                 Let's define the states for **{first}**.\n\
                 What lifecycle states can a {first} be in?\n\n\
                 For example, an Order might have: Draft, Submitted, Confirmed, Shipped, Delivered, Cancelled\n\
                 List the states separated by commas."
            ),
            done: true,
        }];
        msgs.extend(self.advance_phase());
        msgs
    }

    fn handle_state_discovery(&mut self, user_msg: &str) -> Vec<WsMessage> {
        let states: Vec<String> = user_msg
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        if states.is_empty() {
            return vec![WsMessage::AgentResponse {
                content: "Please provide at least one state name.".to_string(),
                done: true,
            }];
        }

        // Apply to first entity (or current)
        if let Some(ref mut entity) = self.current_entity {
            entity.states = states
                .iter()
                .enumerate()
                .map(|(i, name)| StateDefinition {
                    name: name.clone(),
                    description: String::new(),
                    is_terminal: i == states.len() - 1,
                })
                .collect();
        }

        let entity_name = self
            .current_entity
            .as_ref()
            .map(|e| e.name.clone())
            .unwrap_or_default();

        let mut msgs = vec![WsMessage::AgentResponse {
            content: format!(
                "States for {entity_name}: {}.\n\n\
                 Now let's define the **actions** (transitions) for {entity_name}.\n\
                 For each action, tell me:\n\
                 - Action name (e.g., SubmitOrder)\n\
                 - From which state(s)\n\
                 - To which state\n\n\
                 Format: ActionName: FromState -> ToState\n\
                 One per line.",
                states.join(", ")
            ),
            done: true,
        }];
        msgs.extend(self.advance_phase());
        msgs
    }

    fn handle_action_discovery(&mut self, user_msg: &str) -> Vec<WsMessage> {
        // Parse actions from "ActionName: FromState -> ToState" format
        for line in user_msg.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(action) = parse_action_line(line) {
                if let Some(ref mut entity) = self.current_entity {
                    entity.actions.push(action);
                }
            }
        }

        let action_count = self
            .current_entity
            .as_ref()
            .map(|e| e.actions.len())
            .unwrap_or(0);

        let mut msgs = vec![WsMessage::AgentResponse {
            content: format!(
                "I've collected {action_count} action(s).\n\n\
                 Do any actions have **guards** (preconditions)?\n\
                 For example: 'SubmitOrder requires items > 0'\n\n\
                 Enter guards as: ActionName requires condition\n\
                 Or type 'none' to skip."
            ),
            done: true,
        }];
        msgs.extend(self.advance_phase());
        msgs
    }

    fn handle_guard_discovery(&mut self, user_msg: &str) -> Vec<WsMessage> {
        let trimmed = user_msg.trim().to_lowercase();
        if trimmed != "none" && trimmed != "skip" {
            // Parse guard definitions: "ActionName requires condition"
            for line in user_msg.lines() {
                let line = line.trim();
                if let Some((action_name, guard_expr)) = line.split_once("requires") {
                    let action_name = action_name.trim();
                    let guard_expr = guard_expr.trim().to_string();
                    if let Some(ref mut entity) = self.current_entity {
                        if let Some(action) =
                            entity.actions.iter_mut().find(|a| a.name == action_name)
                        {
                            action.guard = Some(guard_expr);
                        }
                    }
                }
            }
        }

        let mut msgs = vec![WsMessage::AgentResponse {
            content: "Do you have any **safety invariants** — conditions that must always hold?\n\
                      For example: 'When Submitted, items must be > 0'\n\n\
                      Enter as: InvariantName: when State1, State2 then condition\n\
                      Or type 'none' to skip."
                .to_string(),
            done: true,
        }];
        msgs.extend(self.advance_phase());
        msgs
    }

    fn handle_invariant_discovery(&mut self, user_msg: &str) -> Vec<WsMessage> {
        let trimmed = user_msg.trim().to_lowercase();
        if trimmed != "none" && trimmed != "skip" {
            for line in user_msg.lines() {
                let line = line.trim();
                if let Some(inv) = parse_invariant_line(line) {
                    if let Some(ref mut entity) = self.current_entity {
                        entity.invariants.push(inv);
                    }
                }
            }
        }

        // Finalize current entity
        if let Some(entity) = self.current_entity.take() {
            // Update the entity in the list
            if let Some(pos) = self.entities.iter().position(|e| e.name == entity.name) {
                self.entities[pos] = entity;
            } else {
                self.entities.push(entity);
            }
        }

        // Generate spec preview
        let mut preview = String::from("Here's a preview of the generated specs:\n\n");
        for entity in &self.entities {
            if !entity.states.is_empty() {
                let toml = spec_generator::generate_ioa_toml(entity);
                preview.push_str(&format!("**{} IOA TOML:**\n```toml\n{toml}```\n\n", entity.name));
            }
        }
        preview.push_str("Type 'confirm' to proceed with verification and deployment, or describe any changes.");

        let mut msgs = vec![WsMessage::AgentResponse {
            content: preview,
            done: true,
        }];
        msgs.extend(self.advance_phase());
        msgs
    }

    fn handle_spec_review(&mut self, user_msg: &str) -> Vec<WsMessage> {
        let trimmed = user_msg.trim().to_lowercase();
        if trimmed != "confirm" && trimmed != "yes" && trimmed != "ok" && trimmed != "lgtm" {
            return vec![WsMessage::AgentResponse {
                content: "Please type 'confirm' to proceed, or describe what you'd like to change."
                    .to_string(),
                done: true,
            }];
        }

        // Generate and emit all specs
        let mut msgs = Vec::new();

        for entity in &self.entities {
            if entity.states.is_empty() {
                continue;
            }

            let ioa_toml = spec_generator::generate_ioa_toml(entity);
            msgs.push(WsMessage::SpecUpdate {
                spec_type: SpecType::IoaToml,
                content: ioa_toml,
                entity_name: entity.name.clone(),
            });

            let cedar = spec_generator::generate_cedar_policies(entity);
            msgs.push(WsMessage::SpecUpdate {
                spec_type: SpecType::Cedar,
                content: cedar,
                entity_name: entity.name.clone(),
            });
        }

        // CSDL covers all entities
        let defined: Vec<&EntityModel> = self.entities.iter().filter(|e| !e.states.is_empty()).collect();
        if !defined.is_empty() {
            let csdl = spec_generator::generate_csdl_xml(
                &defined.into_iter().cloned().collect::<Vec<_>>(),
                &format!("Temper.{}", self.tenant_name),
            );
            msgs.push(WsMessage::SpecUpdate {
                spec_type: SpecType::CsdlXml,
                content: csdl,
                entity_name: "schema".to_string(),
            });
        }

        msgs.push(WsMessage::AgentResponse {
            content: "Specs generated! Starting verification cascade...".to_string(),
            done: true,
        });
        msgs.extend(self.advance_phase());

        msgs
    }

    /// Get the system prompt for the current phase.
    fn system_prompt(&self) -> String {
        let base = format!(
            "You are a Temper platform developer assistant helping design the '{}' application. \
             You are in the {} phase of the interview.",
            self.tenant_name,
            self.phase.display_name()
        );

        let phase_instructions = match &self.phase {
            InterviewPhase::Welcome => {
                "Greet the developer and ask them to describe their application. \
                 Then help identify the main entity types."
            }
            InterviewPhase::EntityDiscovery => {
                "Help the developer identify entity types. Ask clarifying questions. \
                 Suggest common entities based on the domain. Output entity names."
            }
            InterviewPhase::StateDiscovery => {
                "For each entity, help discover lifecycle states. \
                 Suggest common patterns (Draft->Active->Completed). \
                 Ask about terminal states."
            }
            InterviewPhase::ActionDiscovery => {
                "Help define actions/transitions between states. \
                 For each action identify: name, from-states, to-state, parameters. \
                 Classify as input (user-facing), internal (system), or output (events)."
            }
            InterviewPhase::GuardDiscovery => {
                "Help identify preconditions/guards for actions. \
                 Ask about required state variables (counters, booleans). \
                 Guards are expressions like 'items > 0' or 'has_address = true'."
            }
            InterviewPhase::InvariantDiscovery => {
                "Help identify safety invariants that must always hold. \
                 For example: 'When Submitted, items must be > 0'. \
                 Ask about business rules that should never be violated."
            }
            InterviewPhase::SpecReview => {
                "Show the generated IOA TOML spec to the developer. \
                 Ask for confirmation or changes. \
                 Explain what each section means."
            }
            _ => "",
        };

        format!("{base}\n\n{phase_instructions}")
    }

    /// Advance to the next interview phase, returning phase-update messages.
    fn advance_phase(&mut self) -> Vec<WsMessage> {
        if let Some(next) = self.phase.next() {
            self.phase = next;
            vec![WsMessage::PhaseUpdate {
                phase: self.phase.display_name().to_string(),
                progress: self.phase.progress_percent(),
            }]
        } else {
            vec![]
        }
    }
}

/// Parse an action definition line: "ActionName: FromState -> ToState"
fn parse_action_line(line: &str) -> Option<ActionDefinition> {
    let (name, rest) = line.split_once(':')?;
    let name = name.trim().to_string();

    if let Some((from_part, to_part)) = rest.split_once("->") {
        let from_states: Vec<String> = from_part
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let to_state = to_part.trim().to_string();
        let to_state = if to_state.is_empty() {
            None
        } else {
            Some(to_state)
        };

        Some(ActionDefinition {
            name,
            from_states,
            to_state,
            guard: None,
            params: Vec::new(),
            hint: None,
            kind: ActionKind::Internal,
        })
    } else {
        // No arrow, just an action name from some states
        let from_states: Vec<String> = rest
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        Some(ActionDefinition {
            name,
            from_states,
            to_state: None,
            guard: None,
            params: Vec::new(),
            hint: None,
            kind: ActionKind::Input,
        })
    }
}

/// Parse an invariant definition line: "Name: when State1, State2 then condition"
fn parse_invariant_line(line: &str) -> Option<InvariantDefinition> {
    let (name, rest) = line.split_once(':')?;
    let name = name.trim().to_string();

    if let Some((when_part, assert_part)) = rest.split_once("then") {
        let when_str = when_part.trim().strip_prefix("when").unwrap_or(when_part.trim());
        let when: Vec<String> = when_str
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let assertion = assert_part.trim().to_string();

        Some(InvariantDefinition {
            name,
            when,
            assertion,
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_developer_agent_creation() {
        let agent = DeveloperAgent::new("my-app".to_string(), None);
        assert_eq!(*agent.phase(), InterviewPhase::Welcome);
        assert!(agent.entities().is_empty());
        assert_eq!(agent.turn_number(), 0);
    }

    #[tokio::test]
    async fn test_developer_agent_handle_welcome() {
        let mut agent = DeveloperAgent::new("my-app".to_string(), None);
        let msgs = agent.handle_message("I want to build a project tracker").await;

        // Should get a greeting and phase update
        assert!(!msgs.is_empty());
        let has_response = msgs.iter().any(|m| matches!(m, WsMessage::AgentResponse { .. }));
        assert!(has_response, "Should contain an AgentResponse");
        let has_phase = msgs.iter().any(|m| matches!(m, WsMessage::PhaseUpdate { .. }));
        assert!(has_phase, "Should contain a PhaseUpdate");

        // Should advance to EntityDiscovery
        assert_eq!(*agent.phase(), InterviewPhase::EntityDiscovery);
    }

    #[tokio::test]
    async fn test_developer_agent_collects_entities() {
        let mut agent = DeveloperAgent::new("tracker".to_string(), None);

        // Welcome
        agent.handle_message("build me a task tracker").await;
        assert_eq!(*agent.phase(), InterviewPhase::EntityDiscovery);

        // Entity discovery
        agent.handle_message("Task, Project").await;
        assert_eq!(agent.entities().len(), 2);
        assert_eq!(agent.entities()[0].name, "Task");
        assert_eq!(agent.entities()[1].name, "Project");
        assert_eq!(*agent.phase(), InterviewPhase::StateDiscovery);
    }

    #[tokio::test]
    async fn test_developer_agent_full_flow() {
        let mut agent = DeveloperAgent::new("tracker".to_string(), None);

        // Welcome
        agent.handle_message("task tracker").await;

        // Entity discovery
        agent.handle_message("Task").await;

        // State discovery
        agent.handle_message("Open, InProgress, Done").await;
        assert_eq!(*agent.phase(), InterviewPhase::ActionDiscovery);

        // Action discovery
        agent.handle_message("StartTask: Open -> InProgress\nCompleteTask: InProgress -> Done").await;
        assert_eq!(*agent.phase(), InterviewPhase::GuardDiscovery);

        // Guard discovery
        agent.handle_message("none").await;
        assert_eq!(*agent.phase(), InterviewPhase::InvariantDiscovery);

        // Invariant discovery
        agent.handle_message("none").await;
        assert_eq!(*agent.phase(), InterviewPhase::SpecReview);

        // Verify entity was built correctly
        assert_eq!(agent.entities().len(), 1);
        let task = &agent.entities()[0];
        assert_eq!(task.name, "Task");
        assert_eq!(task.states.len(), 3);
        assert_eq!(task.actions.len(), 2);
    }

    #[tokio::test]
    async fn test_developer_agent_generates_specs() {
        let mut agent = DeveloperAgent::new("tracker".to_string(), None);

        // Drive through to SpecReview
        agent.handle_message("task tracker").await;
        agent.handle_message("Task").await;
        agent.handle_message("Open, InProgress, Done").await;
        agent.handle_message("StartTask: Open -> InProgress\nCompleteTask: InProgress -> Done").await;
        agent.handle_message("none").await;
        agent.handle_message("none").await;
        assert_eq!(*agent.phase(), InterviewPhase::SpecReview);

        // Confirm
        let msgs = agent.handle_message("confirm").await;

        // Should have SpecUpdate messages
        let spec_updates: Vec<_> = msgs
            .iter()
            .filter(|m| matches!(m, WsMessage::SpecUpdate { .. }))
            .collect();
        assert!(
            !spec_updates.is_empty(),
            "Should generate SpecUpdate messages on confirm"
        );

        // Should have IOA TOML
        let has_ioa = spec_updates.iter().any(|m| matches!(m, WsMessage::SpecUpdate { spec_type: SpecType::IoaToml, .. }));
        assert!(has_ioa, "Should generate IOA TOML spec");

        // Should have Cedar
        let has_cedar = spec_updates.iter().any(|m| matches!(m, WsMessage::SpecUpdate { spec_type: SpecType::Cedar, .. }));
        assert!(has_cedar, "Should generate Cedar policies");

        // Should have CSDL
        let has_csdl = spec_updates.iter().any(|m| matches!(m, WsMessage::SpecUpdate { spec_type: SpecType::CsdlXml, .. }));
        assert!(has_csdl, "Should generate CSDL XML");
    }

    #[test]
    fn test_parse_action_line() {
        let action = parse_action_line("SubmitOrder: Draft -> Submitted").unwrap();
        assert_eq!(action.name, "SubmitOrder");
        assert_eq!(action.from_states, vec!["Draft"]);
        assert_eq!(action.to_state, Some("Submitted".to_string()));
    }

    #[test]
    fn test_parse_action_line_multiple_from() {
        let action = parse_action_line("Cancel: Draft, Submitted -> Cancelled").unwrap();
        assert_eq!(action.name, "Cancel");
        assert_eq!(action.from_states, vec!["Draft", "Submitted"]);
        assert_eq!(action.to_state, Some("Cancelled".to_string()));
    }

    #[test]
    fn test_parse_invariant_line() {
        let inv =
            parse_invariant_line("ItemsRequired: when Submitted, Confirmed then items > 0")
                .unwrap();
        assert_eq!(inv.name, "ItemsRequired");
        assert_eq!(inv.when, vec!["Submitted", "Confirmed"]);
        assert_eq!(inv.assertion, "items > 0");
    }

    #[tokio::test]
    async fn test_developer_agent_increments_turn() {
        let mut agent = DeveloperAgent::new("tracker".to_string(), None);
        assert_eq!(agent.turn_number(), 0);

        // Welcome
        agent.handle_message("build me a task tracker").await;
        assert_eq!(agent.turn_number(), 1);

        // Entity discovery
        agent.handle_message("Task").await;
        assert_eq!(agent.turn_number(), 2);

        // State discovery
        agent.handle_message("Open, InProgress, Done").await;
        assert_eq!(agent.turn_number(), 3);
    }
}
