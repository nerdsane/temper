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
use effects::{parse_effect_fields, parse_effect_value};
#[cfg(test)]
use guards::parse_guard_clause;
use guards::{parse_guard_fields, parse_guard_value};
use inline::{join_multiline_arrays, parse_action_params, parse_kv, parse_string_array};

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
    FieldInvariant,
    StateTimeout,
    /// ADR-0181: parsed by serde in an isolated second pass.
    CollectionWorkflow,
    /// ADR-0153: `[[key]]` unique-key declarations. Passthrough; extracted via
    /// serde in the second pass.
    Key,
    /// ADR-0155: `[[vector]]` vector access-path declarations. Passthrough;
    /// extracted via serde in the second pass.
    Vector,
    Webhook,
    /// ADR-0046: nested `[[action.triggers]]` blocks. Hand-rolled parser
    /// skips the body; triggers are extracted via serde in the second pass
    /// and merged into their action by name.
    ActionTrigger,
    /// ADR-0040: nested composite-action metadata blocks. Hand-rolled parser
    /// skips the body; metadata is extracted via serde in the second pass.
    CompositeActionMetadata,
    /// Canonical TOML emits inline action guards and effects as nested
    /// array-of-table sections. Serde restores those typed values in a second
    /// pass so their fields cannot leak into the parent action.
    ActionBehaviorMetadata,
}

#[derive(Debug, Default)]
struct ParseState {
    meta_name: String,
    meta_states: Vec<String>,
    meta_initial: String,
    meta_allow_indefinite_states: Vec<String>,
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
            "[[field_invariant]]" => self.start_passthrough_section(Section::FieldInvariant),
            // ADR-0049: state_timeouts use nested inline tables for params;
            // parse via serde in the second pass rather than field-by-field.
            "[[state_timeout]]" => self.start_passthrough_section(Section::StateTimeout),
            "[[collection_workflow]]" => {
                self.start_passthrough_section(Section::CollectionWorkflow)
            }
            // ADR-0153: [[key]] unique-key declarations — passthrough; serde
            // extracts them in the second pass.
            "[[key]]" => self.start_passthrough_section(Section::Key),
            // ADR-0155: [[vector]] access-path declarations — passthrough; serde
            // extracts them in the second pass.
            "[[vector]]" => self.start_passthrough_section(Section::Vector),
            "[[webhook]]" => self.start_webhook_section(),
            _ if line.starts_with("[webhook.") => self.start_webhook_section(),
            // ADR-0046: nested [[action.triggers]] — flush the action body so
            // trigger keys don't leak into its fields, then enter passthrough
            // (serde extracts triggers in the second pass).
            "[[action.triggers]]" => {
                self.flush_items();
                self.current_section = Section::ActionTrigger;
                true
            }
            "[[action.cedar_gate]]" | "[[action.sub_writes]]" => {
                self.flush_items();
                self.current_section = Section::CompositeActionMetadata;
                true
            }
            "[[action.guard]]" | "[[action.effect]]" => {
                self.flush_items();
                self.current_section = Section::ActionBehaviorMetadata;
                true
            }
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
            Section::FieldInvariant
            | Section::StateTimeout
            | Section::CollectionWorkflow
            | Section::Key
            | Section::Vector
            | Section::Webhook
            | Section::ActionTrigger
            | Section::CompositeActionMetadata
            | Section::ActionBehaviorMetadata
            | Section::None => {}
        }

        Ok(())
    }

    fn finish(mut self, input: &str) -> Result<Automaton, AutomatonParseError> {
        self.flush_items();
        self.flush_integration();

        debug_assert!(self.current_action.is_none());
        debug_assert!(self.current_invariant.is_none());
        debug_assert!(self.current_state_var.is_none());
        debug_assert!(self.current_liveness.is_none());
        debug_assert!(self.current_integration.is_none());

        // ADR-0046: extract [[action.triggers]] via serde and merge into
        // actions by name. The hand-rolled parser skips these blocks.
        let mut triggers_by_action = extract_action_triggers(input)?;
        let mut composite_by_action = extract_action_composite_metadata(input)?;
        let mut behavior_by_action = extract_action_behavior_metadata(input)?;
        let mut actions = self.actions;
        for action in &mut actions {
            if let Some(trigs) = triggers_by_action.remove(&action.name) {
                action.triggers.extend(trigs);
            }
            if let Some(metadata) = composite_by_action.remove(&action.name) {
                action.cedar_gate = metadata.cedar_gate;
                action.sub_writes.extend(metadata.sub_writes);
            }
            if let Some(metadata) = behavior_by_action.remove(&action.name) {
                action.guard.extend(metadata.guards);
                action.effect.extend(metadata.effects);
            }
        }

        Ok(Automaton {
            automaton: AutomatonMeta {
                name: self.meta_name,
                states: self.meta_states,
                initial: self.meta_initial,
                allow_indefinite_states: self.meta_allow_indefinite_states,
            },
            state: self.state_vars,
            actions,
            invariants: self.invariants,
            liveness: self.liveness_props,
            integrations: self.integrations,
            webhooks: extract_webhooks(input),
            context_entities: Vec::new(),
            field_invariants: Vec::new(),
            state_timeouts: Vec::new(),
            collection_workflows: Vec::new(),
            keys: Vec::new(),
            vectors: Vec::new(),
            admission: None,
        })
    }

    fn apply_automaton_field(&mut self, key: &str, value: &str) {
        match key {
            "name" => self.meta_name = value.to_string(),
            "initial" => self.meta_initial = value.to_string(),
            "states" => self.meta_states = parse_string_array(value),
            // ADR-0050: allowlist of states permitted to be indefinite.
            "allow_indefinite_states" => {
                self.meta_allow_indefinite_states = parse_string_array(value);
            }
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
            "entity_type" => state_var.entity_type = Some(value.to_string()),
            "initial" => state_var.initial = value.to_string(),
            // ADR-0045 / ADR-0047: per-field overflow knobs.
            "overflow_inline_max_bytes" => {
                if let Ok(v) = value.parse::<usize>() {
                    state_var.overflow_inline_max_bytes = Some(v);
                }
            }
            "overflow_ttl_seconds" => {
                if let Ok(v) = value.parse::<u64>() {
                    state_var.overflow_ttl_seconds = Some(v);
                }
            }
            "query_indexed" => match value.trim() {
                "true" => state_var.query_indexed = Some(true),
                "false" => state_var.query_indexed = Some(false),
                _ => {}
            },
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
            "params" => action.params = parse_action_params(value),
            "hint" => action.hint = Some(value.to_string()),
            "record_parent_event" => match value.trim() {
                "true" => action.record_parent_event = true,
                "false" => action.record_parent_event = false,
                _ => {}
            },
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
            "llm" => integration.llm = value == "true",
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
            entity_type: None,
            initial: String::new(),
            overflow_inline_max_bytes: None,
            overflow_ttl_seconds: None,
            query_indexed: None,
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
            record_parent_event: true,
            triggers: Vec::new(),
            cedar_gate: None,
            sub_writes: Vec::new(),
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
            failure_routes: Vec::new(),
            llm: false,
            config: std::collections::BTreeMap::new(),
        });
        self.current_section = Section::Integration;
        true
    }

    fn start_webhook_section(&mut self) -> bool {
        self.start_passthrough_section(Section::Webhook)
    }

    fn start_passthrough_section(&mut self, section: Section) -> bool {
        self.flush_items();
        self.flush_integration();
        self.current_section = section;
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

    let mut automaton = state.finish(input)?;
    // Field invariants use nested inline-table predicates that the hand-rolled
    // parser does not handle, so delegate to serde. Unlike webhooks and agent
    // triggers, parse errors are surfaced — a silently-dropped field invariant
    // means the constraint is not enforced, which is a safety bug.
    automaton.field_invariants = extract_field_invariants(input)?;
    // ADR-0049: state_timeouts use nested params tables; parse via serde in
    // an isolated pass. Errors are propagated — a silently-dropped timeout
    // would mean a trap state at runtime.
    automaton.state_timeouts = extract_state_timeouts(input)?;
    // ADR-0181: collection declarations are visible to verification even
    // while production TransitionTable activation remains gated.
    automaton.collection_workflows = extract_collection_workflows(input)?;
    // ADR-0153: [[key]] unique-key declarations; serde-extracted like timeouts.
    // A silently-dropped key would mean the declared access path is not indexed.
    automaton.keys = extract_keys(input)?;
    // ADR-0155: [[vector]] access-path declarations; serde-extracted like keys.
    // A silently-dropped vector path would leave similarity unindexed.
    automaton.vectors = extract_vectors(input)?;
    // ADR-0051: optional [admission] block.
    automaton.admission = extract_admission(input)?;
    Ok(automaton)
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

/// Extract nested `[[action.triggers]]` sections via serde (ADR-0046).
///
/// Returns a map from action name to the triggers declared under it.
/// The hand-rolled parser is unable to handle nested array-of-tables,
/// so we do a second pass with `toml::from_str` over only the `[[action]]`
/// sections. Errors are propagated: silently dropping malformed triggers
/// would change runtime orchestration behavior.
fn extract_action_triggers(
    source: &str,
) -> Result<std::collections::BTreeMap<String, Vec<super::types::ActionTrigger>>, AutomatonParseError>
{
    let slice = isolate_action_sections(source);
    if slice.trim().is_empty() {
        return Ok(std::collections::BTreeMap::new());
    }

    #[derive(serde::Deserialize)]
    struct ActionTriggersWrapper {
        #[serde(default, rename = "action")]
        actions: Vec<ActionSkeleton>,
    }
    #[derive(serde::Deserialize)]
    struct ActionSkeleton {
        #[serde(default)]
        name: String,
        #[serde(default)]
        triggers: Vec<super::types::ActionTrigger>,
    }
    let wrapper: ActionTriggersWrapper = toml::from_str(&slice)
        .map_err(|e| AutomatonParseError::Toml(format!("action.triggers: {e}")))?;
    let mut map: std::collections::BTreeMap<String, Vec<super::types::ActionTrigger>> =
        std::collections::BTreeMap::new();
    for action in wrapper.actions {
        if action.name.is_empty() || action.triggers.is_empty() {
            continue;
        }
        map.entry(action.name).or_default().extend(action.triggers);
    }
    Ok(map)
}

#[derive(Debug, Default)]
struct ParsedCompositeActionMetadata {
    cedar_gate: Option<super::types::CompositeCedarGate>,
    sub_writes: Vec<super::types::SubWriteSpec>,
}

#[derive(Debug, Default)]
struct ParsedActionBehaviorMetadata {
    guards: Vec<super::types::Guard>,
    effects: Vec<super::types::Effect>,
}

/// Extract canonical `[[action.guard]]` and `[[action.effect]]` sections.
///
/// Source-authored IOA usually expresses these values as inline arrays, which
/// the hand-written parser handles directly. The canonical TOML serializer
/// expands them into nested array-of-table sections instead. This filtered
/// serde pass retains only each parent action name and those expanded sections,
/// preserving both forms without asking serde to parse legacy string effects.
fn extract_action_behavior_metadata(
    source: &str,
) -> Result<std::collections::BTreeMap<String, ParsedActionBehaviorMetadata>, AutomatonParseError> {
    let slice = isolate_action_behavior_sections(source);
    if slice.trim().is_empty() {
        return Ok(std::collections::BTreeMap::new());
    }

    #[derive(serde::Deserialize)]
    struct ActionBehaviorWrapper {
        #[serde(default, rename = "action")]
        actions: Vec<ActionSkeleton>,
    }
    #[derive(serde::Deserialize)]
    struct ActionSkeleton {
        #[serde(default)]
        name: String,
        #[serde(default)]
        guard: Vec<toml::Value>,
        #[serde(default)]
        effect: Vec<toml::Value>,
    }

    let wrapper: ActionBehaviorWrapper = toml::from_str(&slice)
        .map_err(|error| AutomatonParseError::Toml(format!("action behavior: {error}")))?;
    let mut map = std::collections::BTreeMap::<String, ParsedActionBehaviorMetadata>::new();
    for action in wrapper.actions {
        if action.name.is_empty() || (action.guard.is_empty() && action.effect.is_empty()) {
            continue;
        }
        let metadata = map.entry(action.name).or_default();
        for value in action.guard {
            let fields = canonical_behavior_fields(value, "guard")?;
            metadata.guards.push(parse_guard_fields(&fields)?);
        }
        for value in action.effect {
            let fields = canonical_behavior_fields(value, "effect")?;
            if let Some(effect) = parse_effect_fields(&fields)? {
                metadata.effects.push(effect);
            }
        }
    }
    Ok(map)
}

fn canonical_behavior_fields(
    value: toml::Value,
    behavior: &str,
) -> Result<std::collections::BTreeMap<String, String>, AutomatonParseError> {
    let toml::Value::Table(table) = value else {
        return Err(AutomatonParseError::Toml(format!(
            "action behavior: {behavior} entry must be a table"
        )));
    };
    Ok(table
        .into_iter()
        .map(|(key, value)| (key, canonical_behavior_field(value)))
        .collect())
}

fn canonical_behavior_field(value: toml::Value) -> String {
    match value {
        toml::Value::String(value) => value,
        toml::Value::Integer(value) => value.to_string(),
        toml::Value::Float(value) => value.to_string(),
        toml::Value::Boolean(value) => value.to_string(),
        toml::Value::Datetime(value) => value.to_string(),
        toml::Value::Array(values) => values
            .into_iter()
            .map(canonical_behavior_field)
            .collect::<Vec<_>>()
            .join(","),
        toml::Value::Table(table) => toml::Value::Table(table).to_string(),
    }
}

/// Extract nested `[[action.cedar_gate]]` and `[[action.sub_writes]]`
/// sections via serde (ADR-0040).
fn extract_action_composite_metadata(
    source: &str,
) -> Result<std::collections::BTreeMap<String, ParsedCompositeActionMetadata>, AutomatonParseError>
{
    let slice = isolate_action_sections(source);
    if slice.trim().is_empty() {
        return Ok(std::collections::BTreeMap::new());
    }

    #[derive(serde::Deserialize)]
    struct ActionCompositeWrapper {
        #[serde(default, rename = "action")]
        actions: Vec<ActionSkeleton>,
    }
    #[derive(serde::Deserialize)]
    struct ActionSkeleton {
        #[serde(default)]
        name: String,
        #[serde(default)]
        cedar_gate: Vec<super::types::CompositeCedarGate>,
        #[serde(default)]
        sub_writes: Vec<super::types::SubWriteSpec>,
    }
    let wrapper: ActionCompositeWrapper = toml::from_str(&slice)
        .map_err(|e| AutomatonParseError::Toml(format!("action composite metadata: {e}")))?;
    let mut map: std::collections::BTreeMap<String, ParsedCompositeActionMetadata> =
        std::collections::BTreeMap::new();
    for action in wrapper.actions {
        if action.name.is_empty() || (action.cedar_gate.is_empty() && action.sub_writes.is_empty())
        {
            continue;
        }
        let metadata = map.entry(action.name).or_default();
        if let Some(gate) = action.cedar_gate.into_iter().next() {
            metadata.cedar_gate = Some(gate);
        }
        metadata.sub_writes.extend(action.sub_writes);
    }
    Ok(map)
}

/// Extract `[[field_invariant]]` sections from TOML source via serde.
///
/// The hand-written parser does not handle nested inline-table predicates,
/// so we delegate to `toml::from_str` in a second pass. Unlike `extract_webhooks`
/// and `extract_agent_triggers`, parse errors here are propagated — a silently
/// dropped field invariant would mean the constraint is not enforced at
/// runtime, which is worse than a loud parse failure.
///
/// To keep this resilient against unrelated TOML quirks elsewhere in the
/// source (e.g. duplicate keys in integration config that a strict
/// `toml::from_str` on the whole file would reject), we first slice out
/// only the `[[field_invariant]]` sections and parse just those.
fn extract_field_invariants(
    source: &str,
) -> Result<Vec<super::field_invariant::FieldInvariant>, AutomatonParseError> {
    let slice = isolate_field_invariant_sections(source);
    if slice.trim().is_empty() {
        return Ok(Vec::new());
    }
    deserialize_array_section(&slice, "field_invariant")
}

/// Extract the optional `[admission]` block from TOML source via serde
/// (ADR-0051).
///
/// Only one admission block is allowed per entity. The block lives at the
/// top level and accepts inline-table overrides, so serde handles it
/// entirely — the hand-rolled parser would need separate handling for the
/// `max_concurrent_actions = { ... }` inline table otherwise.
fn extract_admission(source: &str) -> Result<Option<super::types::Admission>, AutomatonParseError> {
    let slice = isolate_single_table(source, "[admission]");
    if slice.trim().is_empty() {
        return Ok(None);
    }

    #[derive(serde::Deserialize)]
    struct AdmissionWrapper {
        admission: super::types::Admission,
    }
    toml::from_str::<AdmissionWrapper>(&slice)
        .map(|w| Some(w.admission))
        .map_err(|e| AutomatonParseError::Toml(format!("admission: {e}")))
}

/// Return a minimal TOML document containing only the single-table
/// `[header]` block (e.g., `[admission]`) from `source`. Other top-level
/// sections are skipped. Used for single-instance configuration blocks
/// where array-of-tables semantics do not apply.
fn isolate_single_table(source: &str, marker: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for line in source.lines() {
        let trimmed = line.trim_start();
        let is_header = trimmed.starts_with('[');
        if is_header {
            inside = trimmed.starts_with(marker);
            if inside {
                out.push_str(marker);
                out.push('\n');
            }
            continue;
        }
        if inside {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Return a minimal TOML document containing only the sections with the
/// given `marker` header (e.g. `"[[state_timeout]]"`) from `source`. Other
/// top-level sections are skipped; content inside target sections is copied
/// verbatim so inline tables (`params = { ... }`) parse correctly.
fn isolate_sections(source: &str, marker: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    let table_name = marker.trim_matches(['[', ']']);
    let nested_table_prefix = format!("[{table_name}.");
    let nested_array_prefix = format!("[[{table_name}.");
    for line in source.lines() {
        let trimmed = line.trim_start();
        let is_header = trimmed.starts_with('[');
        if is_header {
            if trimmed == marker {
                inside = true;
                out.push_str(marker);
                out.push('\n');
            } else if inside
                && (trimmed.starts_with(&nested_table_prefix)
                    || trimmed.starts_with(&nested_array_prefix))
            {
                out.push_str(trimmed);
                out.push('\n');
            } else {
                inside = false;
            }
            continue;
        }
        if inside {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Return a minimal TOML document containing only `[[action]]` sections and
/// their nested `[[action.*]]` tables from `source`.
fn isolate_action_sections(source: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for line in source.lines() {
        let trimmed = line.trim_start();
        let is_header = trimmed.starts_with('[');
        if is_header {
            inside = trimmed.starts_with("[[action]]")
                || trimmed.starts_with("[[action.")
                || trimmed.starts_with("[action.");
            if inside {
                out.push_str(trimmed);
                out.push('\n');
            }
            continue;
        }
        if inside {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Return a TOML document containing action names plus only nested guard and
/// effect tables emitted by canonical serialization.
fn isolate_action_behavior_sections(source: &str) -> String {
    #[derive(Clone, Copy)]
    enum CopyMode {
        Skip,
        ActionName,
        Behavior,
    }

    let mut out = String::new();
    let mut mode = CopyMode::Skip;
    for line in source.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            mode = match trimmed {
                "[[action]]" => {
                    out.push_str("[[action]]\n");
                    CopyMode::ActionName
                }
                "[[action.guard]]" | "[[action.effect]]" => {
                    out.push_str(trimmed);
                    out.push('\n');
                    CopyMode::Behavior
                }
                _ => CopyMode::Skip,
            };
            continue;
        }

        match mode {
            CopyMode::ActionName if trimmed.starts_with("name =") => {
                out.push_str(trimmed);
                out.push('\n');
            }
            CopyMode::Behavior => {
                out.push_str(line);
                out.push('\n');
            }
            CopyMode::Skip | CopyMode::ActionName => {}
        }
    }
    out
}

/// Extract `[[state_timeout]]` sections from TOML source via serde
/// (ADR-0049).
///
/// Uses the same isolation pattern as `extract_field_invariants` so
/// unrelated TOML quirks in other sections cannot break parsing. Errors
/// are propagated — a silently dropped state timeout would mean a
/// declared liveness contract is not enforced at runtime.
fn extract_state_timeouts(
    source: &str,
) -> Result<Vec<super::types::StateTimeout>, AutomatonParseError> {
    let slice = isolate_sections(source, "[[state_timeout]]");
    if slice.trim().is_empty() {
        return Ok(Vec::new());
    }

    deserialize_array_section(&slice, "state_timeout")
}

/// Extract `[[collection_workflow]]` declarations through an isolated serde
/// pass. A malformed declaration must never disappear silently.
fn extract_collection_workflows(
    source: &str,
) -> Result<Vec<super::types::CollectionWorkflow>, AutomatonParseError> {
    let slice = isolate_sections(source, "[[collection_workflow]]");
    if slice.trim().is_empty() {
        return Ok(Vec::new());
    }

    #[derive(serde::Deserialize)]
    struct CollectionWorkflowWrapper {
        #[serde(default, rename = "collection_workflow")]
        collection_workflows: Vec<super::types::CollectionWorkflow>,
    }
    toml::from_str::<CollectionWorkflowWrapper>(&slice)
        .map(|wrapper| wrapper.collection_workflows)
        .map_err(|error| AutomatonParseError::Toml(format!("collection_workflow: {error}")))
}

/// Extract `[[key]]` unique-key declarations from TOML source via serde
/// (ADR-0153). Same isolation pattern as `extract_state_timeouts`. Errors are
/// propagated — a silently-dropped key would leave a declared access path
/// unindexed, re-opening the negative-existence scan (the 413, ARN-68).
fn extract_keys(source: &str) -> Result<Vec<super::types::KeyDecl>, AutomatonParseError> {
    let slice = isolate_sections(source, "[[key]]");
    if slice.trim().is_empty() {
        return Ok(Vec::new());
    }

    deserialize_array_section(&slice, "key")
}

/// Extract `[[vector]]` access-path declarations from TOML source via serde
/// (ADR-0155). Same isolation pattern as `extract_keys`. Errors are propagated —
/// a silently-dropped vector path would leave similarity unindexed while the spec
/// author believes `Temper.Nearest` will work.
fn extract_vectors(source: &str) -> Result<Vec<super::types::VectorDecl>, AutomatonParseError> {
    let slice = isolate_sections(source, "[[vector]]");
    if slice.trim().is_empty() {
        return Ok(Vec::new());
    }

    deserialize_array_section(&slice, "vector")
}

fn deserialize_array_section<T>(source: &str, key: &str) -> Result<Vec<T>, AutomatonParseError>
where
    T: serde::de::DeserializeOwned,
{
    let document = toml::from_str::<toml::Value>(source)
        .map_err(|error| AutomatonParseError::Toml(format!("{key}: {error}")))?;
    let Some(items) = document.get(key).and_then(toml::Value::as_array) else {
        return Ok(Vec::new());
    };
    items
        .iter()
        .cloned()
        .map(|item| {
            item.try_into()
                .map_err(|error| AutomatonParseError::Toml(format!("{key}: {error}")))
        })
        .collect()
}

/// Return a minimal TOML document containing only the `[[field_invariant]]`
/// sections from `source`. Any other top-level section is skipped.
///
/// Nested predicate tables emitted by canonical TOML remain attached to their
/// parent invariant. Unrelated top-level sections are dropped.
fn isolate_field_invariant_sections(source: &str) -> String {
    isolate_sections(source, "[[field_invariant]]")
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

#[cfg(test)]
#[path = "nested_metadata_test.rs"]
mod nested_metadata_test;
