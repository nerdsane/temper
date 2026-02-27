//! Minimal TOML parser for I/O Automaton specifications.
//!
//! Handles the subset of TOML used by IOA specs since we use a hand-rolled
//! parser rather than the full `toml` crate for the core parsing. Webhook
//! sections are delegated to `toml::from_str` in a second pass.

use super::parser::AutomatonParseError;
use super::types::*;

/// Parse TOML into an Automaton struct.
///
/// This is a minimal parser that handles the subset of TOML we use:
/// - `[automaton]` table with name, states, initial
/// - `[[action]]` array of tables
/// - `[[invariant]]` array of tables
/// - Simple key = "value" and key = ["array"] syntax
pub(super) fn parse_toml_to_automaton(input: &str) -> Result<Automaton, AutomatonParseError> {
    let mut meta_name = String::new();
    let mut meta_states: Vec<String> = Vec::new();
    let mut meta_initial = String::new();
    let mut state_vars: Vec<StateVar> = Vec::new();
    let mut actions: Vec<Action> = Vec::new();
    let mut invariants: Vec<Invariant> = Vec::new();
    let mut liveness_props: Vec<Liveness> = Vec::new();
    let mut integrations: Vec<Integration> = Vec::new();

    let mut current_section = "";
    let mut current_action: Option<Action> = None;
    let mut current_invariant: Option<Invariant> = None;
    let mut current_state_var: Option<StateVar> = None;
    let mut current_liveness: Option<Liveness> = None;
    let mut current_integration: Option<Integration> = None;

    for line in input.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        // Section headers
        if trimmed == "[automaton]" {
            flush_all(
                &mut current_action,
                &mut actions,
                &mut current_invariant,
                &mut invariants,
                &mut current_state_var,
                &mut state_vars,
                &mut current_liveness,
                &mut liveness_props,
            );
            current_section = "automaton";
            continue;
        }
        if trimmed == "[[state]]" {
            flush_all(
                &mut current_action,
                &mut actions,
                &mut current_invariant,
                &mut invariants,
                &mut current_state_var,
                &mut state_vars,
                &mut current_liveness,
                &mut liveness_props,
            );
            current_state_var = Some(StateVar {
                name: String::new(),
                var_type: "string".into(),
                initial: String::new(),
            });
            current_section = "state";
            continue;
        }
        if trimmed == "[[action]]" {
            flush_all(
                &mut current_action,
                &mut actions,
                &mut current_invariant,
                &mut invariants,
                &mut current_state_var,
                &mut state_vars,
                &mut current_liveness,
                &mut liveness_props,
            );
            current_action = Some(Action {
                name: String::new(),
                kind: "internal".into(),
                from: Vec::new(),
                to: None,
                guard: Vec::new(),
                effect: Vec::new(),
                params: Vec::new(),
                hint: None,
            });
            current_section = "action";
            continue;
        }
        if trimmed == "[[invariant]]" {
            flush_all(
                &mut current_action,
                &mut actions,
                &mut current_invariant,
                &mut invariants,
                &mut current_state_var,
                &mut state_vars,
                &mut current_liveness,
                &mut liveness_props,
            );
            current_invariant = Some(Invariant {
                name: String::new(),
                when: Vec::new(),
                assert: String::new(),
            });
            current_section = "invariant";
            continue;
        }
        if trimmed == "[[liveness]]" {
            flush_all(
                &mut current_action,
                &mut actions,
                &mut current_invariant,
                &mut invariants,
                &mut current_state_var,
                &mut state_vars,
                &mut current_liveness,
                &mut liveness_props,
            );
            if let Some(ig) = current_integration.take()
                && !ig.name.is_empty()
            {
                integrations.push(ig);
            }
            current_liveness = Some(Liveness {
                name: String::new(),
                from: Vec::new(),
                reaches: Vec::new(),
                has_actions: None,
            });
            current_section = "liveness";
            continue;
        }
        if trimmed == "[[integration]]" {
            flush_all(
                &mut current_action,
                &mut actions,
                &mut current_invariant,
                &mut invariants,
                &mut current_state_var,
                &mut state_vars,
                &mut current_liveness,
                &mut liveness_props,
            );
            if let Some(ig) = current_integration.take()
                && !ig.name.is_empty()
            {
                integrations.push(ig);
            }
            current_integration = Some(Integration {
                name: String::new(),
                trigger: String::new(),
                integration_type: "webhook".to_string(),
                module: None,
                on_success: None,
                on_failure: None,
                config: std::collections::BTreeMap::new(),
            });
            current_section = "integration";
            continue;
        }
        if trimmed == "[[webhook]]" || trimmed.starts_with("[webhook.") {
            flush_all(
                &mut current_action,
                &mut actions,
                &mut current_invariant,
                &mut invariants,
                &mut current_state_var,
                &mut state_vars,
                &mut current_liveness,
                &mut liveness_props,
            );
            if let Some(ig) = current_integration.take()
                && !ig.name.is_empty()
            {
                integrations.push(ig);
            }
            current_section = "webhook";
            continue;
        }

        // Key-value pairs
        if let Some((key, value)) = parse_kv(trimmed) {
            match current_section {
                "automaton" => match key {
                    "name" => meta_name = value.clone(),
                    "initial" => meta_initial = value.clone(),
                    "states" => meta_states = parse_string_array(&value),
                    _ => {}
                },
                "state" => {
                    if let Some(ref mut sv) = current_state_var {
                        match key {
                            "name" => sv.name = value.clone(),
                            "type" => sv.var_type = value.clone(),
                            "initial" => sv.initial = value.clone(),
                            _ => {}
                        }
                    }
                }
                "action" => {
                    if let Some(ref mut a) = current_action {
                        match key {
                            "name" => a.name = value.clone(),
                            "kind" => a.kind = value.clone(),
                            "from" => a.from = parse_string_array(&value),
                            "to" => a.to = Some(value.clone()),
                            "params" => a.params = parse_string_array(&value),
                            "hint" => a.hint = Some(value.clone()),
                            "guard" => {
                                a.guard.push(parse_guard_clause(&value)?);
                            }
                            "effect" => {
                                // Format: "increment var" → Increment
                                if value.starts_with("increment ") {
                                    let var = value
                                        .strip_prefix("increment ")
                                        .unwrap_or("")
                                        .trim()
                                        .to_string();
                                    if !var.is_empty() {
                                        a.effect.push(Effect::Increment { var });
                                    }
                                }
                                // Format: "decrement var" → Decrement
                                else if value.starts_with("decrement ") {
                                    let var = value
                                        .strip_prefix("decrement ")
                                        .unwrap_or("")
                                        .trim()
                                        .to_string();
                                    if !var.is_empty() {
                                        a.effect.push(Effect::Decrement { var });
                                    }
                                }
                                // Format: "set var true/false" → SetBool
                                else if value.starts_with("set ") {
                                    let parts: Vec<&str> = value.splitn(3, ' ').collect();
                                    if parts.len() == 3 {
                                        let var = parts[1].to_string();
                                        let val = parts[2].trim() == "true";
                                        a.effect.push(Effect::SetBool { var, value: val });
                                    }
                                }
                                // Format: "emit event_name" → Emit
                                else if value.starts_with("emit ") {
                                    let event = value
                                        .strip_prefix("emit ")
                                        .unwrap_or("")
                                        .trim()
                                        .to_string();
                                    if !event.is_empty() {
                                        a.effect.push(Effect::Emit { event });
                                    }
                                }
                                // Format: "trigger integration_name" → Trigger
                                else if value.starts_with("trigger ") {
                                    let name = value
                                        .strip_prefix("trigger ")
                                        .unwrap_or("")
                                        .trim()
                                        .to_string();
                                    if !name.is_empty() {
                                        a.effect.push(Effect::Trigger { name });
                                    }
                                }
                                // Format: array of inline tables [{ type = "schedule", ... }]
                                else if value.starts_with("[{") || value.starts_with("[\n") {
                                    parse_effect_array(&value, &mut a.effect);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                "invariant" => {
                    if let Some(ref mut inv) = current_invariant {
                        match key {
                            "name" => inv.name = value.clone(),
                            "when" => inv.when = parse_string_array(&value),
                            "assert" => inv.assert = value.clone(),
                            _ => {}
                        }
                    }
                }
                "liveness" => {
                    if let Some(ref mut l) = current_liveness {
                        match key {
                            "name" => l.name = value.clone(),
                            "from" => l.from = parse_string_array(&value),
                            "reaches" => l.reaches = parse_string_array(&value),
                            "has_actions" => l.has_actions = Some(value == "true"),
                            _ => {}
                        }
                    }
                }
                "integration" => {
                    if let Some(ref mut ig) = current_integration {
                        match key {
                            "name" => ig.name = value.clone(),
                            "trigger" => ig.trigger = value.clone(),
                            "type" => ig.integration_type = value.clone(),
                            "module" => ig.module = Some(value.clone()),
                            "on_success" => ig.on_success = Some(value.clone()),
                            "on_failure" => ig.on_failure = Some(value.clone()),
                            _ => {
                                ig.config.insert(key.to_string(), value.clone());
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    flush_all(
        &mut current_action,
        &mut actions,
        &mut current_invariant,
        &mut invariants,
        &mut current_state_var,
        &mut state_vars,
        &mut current_liveness,
        &mut liveness_props,
    );
    if let Some(ig) = current_integration.take()
        && !ig.name.is_empty()
    {
        integrations.push(ig);
    }

    Ok(Automaton {
        automaton: AutomatonMeta {
            name: meta_name,
            states: meta_states,
            initial: meta_initial,
        },
        state: state_vars,
        actions,
        invariants,
        liveness: liveness_props,
        integrations,
        webhooks: extract_webhooks(input),
    })
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

#[allow(clippy::too_many_arguments)]
fn flush_all(
    action: &mut Option<Action>,
    actions: &mut Vec<Action>,
    invariant: &mut Option<Invariant>,
    invariants: &mut Vec<Invariant>,
    state_var: &mut Option<StateVar>,
    state_vars: &mut Vec<StateVar>,
    liveness: &mut Option<Liveness>,
    liveness_props: &mut Vec<Liveness>,
) {
    if let Some(a) = action.take()
        && !a.name.is_empty()
    {
        actions.push(a);
    }
    if let Some(inv) = invariant.take()
        && !inv.name.is_empty()
    {
        invariants.push(inv);
    }
    if let Some(sv) = state_var.take()
        && !sv.name.is_empty()
    {
        state_vars.push(sv);
    }
    if let Some(l) = liveness.take()
        && !l.name.is_empty()
    {
        liveness_props.push(l);
    }
}

pub(super) fn parse_guard_clause(value: &str) -> Result<Guard, AutomatonParseError> {
    let trimmed = value.trim();

    // Infix forms: "<var> > <n>" and "<var> < <n>".
    if let Some((lhs, rhs)) = trimmed.split_once('>') {
        let var = lhs.trim();
        let raw = rhs.trim();
        if var.is_empty() || raw.is_empty() {
            return Err(AutomatonParseError::Validation(format!(
                "invalid guard '{trimmed}' (expected '<var> > <n>')"
            )));
        }
        let n: usize = raw.parse().map_err(|_| {
            AutomatonParseError::Validation(format!(
                "invalid guard '{trimmed}' (right side must be an integer)"
            ))
        })?;
        return Ok(Guard::MinCount {
            var: var.to_string(),
            min: n + 1,
        });
    }
    if let Some((lhs, rhs)) = trimmed.split_once('<') {
        let var = lhs.trim();
        let raw = rhs.trim();
        if var.is_empty() || raw.is_empty() {
            return Err(AutomatonParseError::Validation(format!(
                "invalid guard '{trimmed}' (expected '<var> < <n>')"
            )));
        }
        let max: usize = raw.parse().map_err(|_| {
            AutomatonParseError::Validation(format!(
                "invalid guard '{trimmed}' (right side must be an integer)"
            ))
        })?;
        return Ok(Guard::MaxCount {
            var: var.to_string(),
            max,
        });
    }

    // Prefix forms:
    // - "min <var> <n>"
    // - "max <var> <n>"
    // - "is_true <var>"
    // - "list_contains <var> <value>"
    // - "list_length_min <var> <n>"
    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    if parts.is_empty() {
        return Err(AutomatonParseError::Validation(
            "empty guard clause".to_string(),
        ));
    }

    match parts[0] {
        "min" => {
            if parts.len() != 3 {
                return Err(AutomatonParseError::Validation(format!(
                    "invalid guard '{trimmed}' (expected 'min <var> <n>')"
                )));
            }
            let min: usize = parts[2].parse().map_err(|_| {
                AutomatonParseError::Validation(format!(
                    "invalid guard '{trimmed}' (min must be an integer)"
                ))
            })?;
            Ok(Guard::MinCount {
                var: parts[1].to_string(),
                min,
            })
        }
        "max" => {
            if parts.len() != 3 {
                return Err(AutomatonParseError::Validation(format!(
                    "invalid guard '{trimmed}' (expected 'max <var> <n>')"
                )));
            }
            let max: usize = parts[2].parse().map_err(|_| {
                AutomatonParseError::Validation(format!(
                    "invalid guard '{trimmed}' (max must be an integer)"
                ))
            })?;
            Ok(Guard::MaxCount {
                var: parts[1].to_string(),
                max,
            })
        }
        "is_true" => {
            if parts.len() != 2 {
                return Err(AutomatonParseError::Validation(format!(
                    "invalid guard '{trimmed}' (expected 'is_true <var>')"
                )));
            }
            Ok(Guard::IsTrue {
                var: parts[1].to_string(),
            })
        }
        "list_contains" => {
            if parts.len() < 3 {
                return Err(AutomatonParseError::Validation(format!(
                    "invalid guard '{trimmed}' (expected 'list_contains <var> <value>')"
                )));
            }
            Ok(Guard::ListContains {
                var: parts[1].to_string(),
                value: parts[2..].join(" "),
            })
        }
        "list_length_min" => {
            if parts.len() != 3 {
                return Err(AutomatonParseError::Validation(format!(
                    "invalid guard '{trimmed}' (expected 'list_length_min <var> <n>')"
                )));
            }
            let min: usize = parts[2].parse().map_err(|_| {
                AutomatonParseError::Validation(format!(
                    "invalid guard '{trimmed}' (min must be an integer)"
                ))
            })?;
            Ok(Guard::ListLengthMin {
                var: parts[1].to_string(),
                min,
            })
        }
        _ => Err(AutomatonParseError::Validation(format!(
            "unsupported guard syntax '{trimmed}'"
        ))),
    }
}

/// Parse an effect array in inline table format.
///
/// Handles: `[{ type = "schedule", action = "Refresh", delay_seconds = 2700 }]`
fn parse_effect_array(value: &str, effects: &mut Vec<Effect>) {
    let trimmed = value.trim();
    if !trimmed.starts_with('[') || !trimmed.ends_with(']') {
        return;
    }
    let inner = &trimmed[1..trimmed.len() - 1];

    // Split on "}, {" to separate inline table entries.
    // Simple approach: iterate over inline tables delimited by braces.
    for entry in split_inline_tables(inner) {
        let entry = entry.trim().trim_matches('{').trim_matches('}').trim();
        let fields = parse_inline_fields(entry);

        let effect_type = fields.get("type").map(|s| s.as_str()).unwrap_or("");
        match effect_type {
            "schedule" => {
                let action = fields.get("action").cloned().unwrap_or_default();
                let delay_seconds: u64 = fields
                    .get("delay_seconds")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                if !action.is_empty() {
                    effects.push(Effect::Schedule {
                        action,
                        delay_seconds,
                    });
                }
            }
            "increment" => {
                if let Some(var) = fields.get("var").cloned() {
                    effects.push(Effect::Increment { var });
                }
            }
            "decrement" => {
                if let Some(var) = fields.get("var").cloned() {
                    effects.push(Effect::Decrement { var });
                }
            }
            "set_bool" => {
                if let Some(var) = fields.get("var").cloned() {
                    let value = fields.get("value").map(|s| s == "true").unwrap_or(false);
                    effects.push(Effect::SetBool { var, value });
                }
            }
            "emit" => {
                if let Some(event) = fields.get("event").cloned() {
                    effects.push(Effect::Emit { event });
                }
            }
            "trigger" => {
                if let Some(name) = fields.get("name").cloned() {
                    effects.push(Effect::Trigger { name });
                }
            }
            _ => {}
        }
    }
}

/// Split inline tables from a TOML array (e.g., "{a = 1}, {b = 2}").
fn split_inline_tables(s: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let mut depth = 0;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    result.push(&s[start..=i]);
                    start = i + 1;
                }
            }
            _ => {}
        }
    }
    result
}

/// Parse key-value pairs from an inline table (e.g., "type = "schedule", action = "Refresh"").
fn parse_inline_fields(s: &str) -> std::collections::BTreeMap<String, String> {
    let mut map = std::collections::BTreeMap::new();
    for pair in s.split(',') {
        let pair = pair.trim();
        if let Some(eq_pos) = pair.find('=') {
            let key = pair[..eq_pos].trim().to_string();
            let val = pair[eq_pos + 1..]
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .to_string();
            map.insert(key, val);
        }
    }
    map
}

pub(super) fn parse_kv(line: &str) -> Option<(&str, String)> {
    let eq = line.find('=')?;
    let key = line[..eq].trim();
    let raw_value = line[eq + 1..].trim();
    let value = raw_value.trim_matches('"').trim_matches('\'').to_string();
    Some((key, value))
}

pub(super) fn parse_string_array(value: &str) -> Vec<String> {
    let trimmed = value.trim();
    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        let inner = &trimmed[1..trimmed.len() - 1];
        inner
            .split(',')
            .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
            .filter(|s| !s.is_empty())
            .collect()
    } else {
        vec![trimmed.trim_matches('"').to_string()]
    }
}
