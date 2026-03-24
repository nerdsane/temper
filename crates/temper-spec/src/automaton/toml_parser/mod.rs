//! Minimal TOML parser for I/O Automaton specifications.
//!
//! Handles the subset of TOML used by IOA specs since we use a hand-rolled
//! parser rather than the full `toml` crate for the core parsing. Webhook
//! sections are delegated to `toml::from_str` in a second pass.

mod effects;
mod guards;
mod inline;

use super::parser::AutomatonParseError;
use super::types::*;
use effects::parse_effect_value;
#[cfg(test)]
use guards::parse_guard_clause;
use guards::parse_guard_value;
use inline::{join_multiline_arrays, parse_kv, parse_string_array};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Section {
    #[default]
    None,
    Automaton,
    State,
    Action,
    Invariant,
    Liveness,
    Integration,
    Webhook,
}

#[derive(Debug, Default)]
struct ParseState {
    meta_name: String,
    meta_states: Vec<String>,
    meta_initial: String,
    state_vars: Vec<StateVar>,
    actions: Vec<Action>,
    invariants: Vec<Invariant>,
    liveness_props: Vec<Liveness>,
    integrations: Vec<Integration>,
    current_section: Section,
    current_action: Option<Action>,
    current_invariant: Option<Invariant>,
    current_state_var: Option<StateVar>,
    current_liveness: Option<Liveness>,
    current_integration: Option<Integration>,
}

impl ParseState {
    fn enter_section(&mut self, line: &str) -> bool {
        match line {
            "[automaton]" => self.start_section(Section::Automaton),
            "[[state]]" => self.start_state_section(),
            "[[action]]" => self.start_action_section(),
            "[[invariant]]" => self.start_invariant_section(),
            "[[liveness]]" => self.start_liveness_section(),
            "[[integration]]" => self.start_integration_section(),
            "[[webhook]]" => self.start_webhook_section(),
            _ if line.starts_with("[webhook.") => self.start_webhook_section(),
            _ => false,
        }
    }

    fn apply_kv(&mut self, key: &str, value: String) -> Result<(), AutomatonParseError> {
        match self.current_section {
            Section::Automaton => self.apply_automaton_field(key, &value),
            Section::State => self.apply_state_field(key, &value),
            Section::Action => self.apply_action_field(key, &value)?,
            Section::Invariant => self.apply_invariant_field(key, &value),
            Section::Liveness => self.apply_liveness_field(key, &value),
            Section::Integration => self.apply_integration_field(key, &value),
            Section::Webhook | Section::None => {}
        }

        Ok(())
    }

    fn finish(mut self, input: &str) -> Automaton {
        self.flush_items();
        self.flush_integration();

        debug_assert!(self.current_action.is_none());
        debug_assert!(self.current_invariant.is_none());
        debug_assert!(self.current_state_var.is_none());
        debug_assert!(self.current_liveness.is_none());
        debug_assert!(self.current_integration.is_none());

        Automaton {
            automaton: AutomatonMeta {
                name: self.meta_name,
                states: self.meta_states,
                initial: self.meta_initial,
            },
            state: self.state_vars,
            actions: self.actions,
            invariants: self.invariants,
            liveness: self.liveness_props,
            integrations: self.integrations,
            webhooks: extract_webhooks(input),
            context_entities: Vec::new(),
            agent_triggers: extract_agent_triggers(input),
        }
    }

    fn apply_automaton_field(&mut self, key: &str, value: &str) {
        match key {
            "name" => self.meta_name = value.to_string(),
            "initial" => self.meta_initial = value.to_string(),
            "states" => self.meta_states = parse_string_array(value),
            _ => {}
        }
    }

    fn apply_state_field(&mut self, key: &str, value: &str) {
        let Some(state_var) = self.current_state_var.as_mut() else {
            return;
        };

        match key {
            "name" => state_var.name = value.to_string(),
            "type" => state_var.var_type = value.to_string(),
            "initial" => state_var.initial = value.to_string(),
            _ => {}
        }
    }

    fn apply_action_field(&mut self, key: &str, value: &str) -> Result<(), AutomatonParseError> {
        let Some(action) = self.current_action.as_mut() else {
            return Ok(());
        };

        match key {
            "name" => action.name = value.to_string(),
            "kind" => action.kind = value.to_string(),
            "from" => action.from = parse_string_array(value),
            "to" => action.to = Some(value.to_string()),
            "params" => action.params = parse_string_array(value),
            "hint" => action.hint = Some(value.to_string()),
            "guard" => parse_guard_value(value, &mut action.guard)?,
            "effect" => parse_effect_value(value, &mut action.effect)?,
            _ => {}
        }

        Ok(())
    }

    fn apply_invariant_field(&mut self, key: &str, value: &str) {
        let Some(invariant) = self.current_invariant.as_mut() else {
            return;
        };

        match key {
            "name" => invariant.name = value.to_string(),
            "when" => invariant.when = parse_string_array(value),
            "assert" => invariant.assert = value.to_string(),
            _ => {}
        }
    }

    fn apply_liveness_field(&mut self, key: &str, value: &str) {
        let Some(liveness) = self.current_liveness.as_mut() else {
            return;
        };

        match key {
            "name" => liveness.name = value.to_string(),
            "from" => liveness.from = parse_string_array(value),
            "reaches" => liveness.reaches = parse_string_array(value),
            "has_actions" => liveness.has_actions = Some(value == "true"),
            _ => {}
        }
    }

    fn apply_integration_field(&mut self, key: &str, value: &str) {
        let Some(integration) = self.current_integration.as_mut() else {
            return;
        };

        match key {
            "name" => integration.name = value.to_string(),
            "trigger" => integration.trigger = value.to_string(),
            "type" => integration.integration_type = value.to_string(),
            "module" => integration.module = Some(value.to_string()),
            "on_success" => integration.on_success = Some(value.to_string()),
            "on_failure" => integration.on_failure = Some(value.to_string()),
            _ => {
                integration
                    .config
                    .insert(key.to_string(), value.to_string());
            }
        }
    }

    fn flush_items(&mut self) {
        if let Some(action) = self.current_action.take()
            && !action.name.is_empty()
        {
            self.actions.push(action);
        }

        if let Some(invariant) = self.current_invariant.take()
            && !invariant.name.is_empty()
        {
            self.invariants.push(invariant);
        }

        if let Some(state_var) = self.current_state_var.take()
            && !state_var.name.is_empty()
        {
            self.state_vars.push(state_var);
        }

        if let Some(liveness) = self.current_liveness.take()
            && !liveness.name.is_empty()
        {
            self.liveness_props.push(liveness);
        }
    }

    fn flush_integration(&mut self) {
        if let Some(integration) = self.current_integration.take()
            && !integration.name.is_empty()
        {
            self.integrations.push(integration);
        }
    }

    fn start_section(&mut self, section: Section) -> bool {
        self.flush_items();
        self.current_section = section;
        true
    }

    fn start_state_section(&mut self) -> bool {
        self.flush_items();
        self.current_state_var = Some(StateVar {
            name: String::new(),
            var_type: "string".into(),
            initial: String::new(),
        });
        self.current_section = Section::State;
        true
    }

    fn start_action_section(&mut self) -> bool {
        self.flush_items();
        self.current_action = Some(Action {
            name: String::new(),
            kind: "internal".into(),
            from: Vec::new(),
            to: None,
            guard: Vec::new(),
            effect: Vec::new(),
            params: Vec::new(),
            hint: None,
        });
        self.current_section = Section::Action;
        true
    }

    fn start_invariant_section(&mut self) -> bool {
        self.flush_items();
        self.current_invariant = Some(Invariant {
            name: String::new(),
            when: Vec::new(),
            assert: String::new(),
        });
        self.current_section = Section::Invariant;
        true
    }

    fn start_liveness_section(&mut self) -> bool {
        self.flush_items();
        self.flush_integration();
        self.current_liveness = Some(Liveness {
            name: String::new(),
            from: Vec::new(),
            reaches: Vec::new(),
            has_actions: None,
        });
        self.current_section = Section::Liveness;
        true
    }

    fn start_integration_section(&mut self) -> bool {
        self.flush_items();
        self.flush_integration();
        self.current_integration = Some(Integration {
            name: String::new(),
            trigger: String::new(),
            integration_type: "webhook".to_string(),
            module: None,
            on_success: None,
            on_failure: None,
            config: std::collections::BTreeMap::new(),
        });
        self.current_section = Section::Integration;
        true
    }

    fn start_webhook_section(&mut self) -> bool {
        self.flush_items();
        self.flush_integration();
        self.current_section = Section::Webhook;
        true
    }
}

/// Parse TOML into an Automaton struct.
///
/// This is a minimal parser that handles the subset of TOML we use:
/// - `[automaton]` table with name, states, initial
/// - `[[action]]` array of tables
/// - `[[invariant]]` array of tables
/// - Simple key = "value" and key = ["array"] syntax
pub(super) fn parse_toml_to_automaton(input: &str) -> Result<Automaton, AutomatonParseError> {
    let mut state = ParseState::default();
    let logical_lines = join_multiline_arrays(input);

    for line in logical_lines {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        if state.enter_section(trimmed) {
            continue;
        }

        if let Some((key, value)) = parse_kv(trimmed) {
            state.apply_kv(key, value)?;
        }
    }

    Ok(state.finish(input))
}

/// Extract `[[webhook]]` sections from TOML source via serde.
///
/// The hand-written parser does not handle `[[webhook]]` sections, so
/// we do a second pass with `toml::from_str` to deserialize them.
fn extract_webhooks(source: &str) -> Vec<super::types::Webhook> {
    #[derive(serde::Deserialize)]
    struct WebhookWrapper {
        #[serde(default, rename = "webhook")]
        webhooks: Vec<super::types::Webhook>,
    }
    toml::from_str::<WebhookWrapper>(source)
        .map(|w| w.webhooks)
        .unwrap_or_default()
}

/// Extract `[[agent_trigger]]` sections from TOML source via serde.
fn extract_agent_triggers(source: &str) -> Vec<super::types::AgentTrigger> {
    #[derive(serde::Deserialize)]
    struct AgentTriggerWrapper {
        #[serde(default, rename = "agent_trigger")]
        agent_triggers: Vec<super::types::AgentTrigger>,
    }
    toml::from_str::<AgentTriggerWrapper>(source)
        .map(|w| w.agent_triggers)
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
