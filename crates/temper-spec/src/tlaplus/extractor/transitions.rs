use super::Transition;
use super::source::{extract_inline_set, extract_quoted_string};

pub(super) fn extract_transitions(source: &str, states: &[String]) -> Vec<Transition> {
    let mut transitions = Vec::new();
    let mut current = ActionBuffer::default();

    for line in source.lines() {
        let trimmed = line.trim();

        if is_section_boundary(trimmed) || is_next_state_relation(trimmed) {
            current.finish(states, &mut transitions);
            break;
        }

        if trimmed.starts_with("\\*") {
            continue;
        }

        if is_action_definition(trimmed) {
            current.finish(states, &mut transitions);
            current.start(trimmed);
            continue;
        }

        current.push_line(trimmed, states, &mut transitions);
    }

    current.finish(states, &mut transitions);
    transitions
}

#[derive(Default)]
struct ActionBuffer {
    name: Option<String>,
    guard: String,
    effect: String,
    in_action: bool,
}

impl ActionBuffer {
    fn start(&mut self, definition: &str) {
        if let Some(action_name) = extract_action_name(definition) {
            self.name = Some(action_name);
            self.guard.clear();
            self.effect.clear();
            self.in_action = true;
            if let Some(rest) = definition.split("==").nth(1) {
                categorize_line(rest.trim(), &mut self.guard, &mut self.effect);
            }
        }
    }

    fn push_line(&mut self, line: &str, states: &[String], out: &mut Vec<Transition>) {
        if !self.in_action {
            return;
        }

        if line.contains(" ==") && !line.starts_with("/\\") && !line.starts_with("\\/") {
            self.finish(states, out);
            self.in_action = false;
            return;
        }

        categorize_line(line, &mut self.guard, &mut self.effect);
    }

    fn finish(&mut self, states: &[String], out: &mut Vec<Transition>) {
        if let Some(name) = self.name.take() {
            out.push(build_transition(&name, &self.guard, &self.effect, states));
        }
    }
}

fn is_section_boundary(trimmed: &str) -> bool {
    trimmed.starts_with("\\*")
        && (trimmed.contains("Safety Invariant") || trimmed.contains("Liveness Propert"))
}

fn is_next_state_relation(trimmed: &str) -> bool {
    trimmed.starts_with("Next") && trimmed.contains("==")
}

fn is_action_definition(trimmed: &str) -> bool {
    trimmed.contains(" ==")
        && !trimmed.contains("Statuses ==")
        && !trimmed.contains("States ==")
        && !trimmed.contains("vars ==")
        && !is_guard_definition(trimmed)
        && !trimmed.starts_with("Init ==")
        && !trimmed.starts_with("Next")
        && !trimmed.starts_with("Spec")
        && !trimmed.starts_with("ASSUME")
}

fn is_guard_definition(line: &str) -> bool {
    let name = line.split("==").next().unwrap_or("").trim();
    name.starts_with("Can") && !name.contains('(')
}

fn extract_action_name(trimmed: &str) -> Option<String> {
    let name_part = trimmed.split("==").next().unwrap_or("").trim();
    let clean = name_part.split('(').next().unwrap_or(name_part).trim();
    if !clean.is_empty() && clean.chars().next().is_some_and(|ch| ch.is_uppercase()) {
        Some(name_part.to_string())
    } else {
        None
    }
}

fn categorize_line(line: &str, guard: &mut String, effect: &mut String) {
    let cleaned = line.trim().trim_start_matches("/\\").trim();
    let target =
        if cleaned.contains("UNCHANGED") || cleaned.contains("' =") || cleaned.contains("'=") {
            effect
        } else {
            guard
        };
    target.push_str(cleaned);
    target.push('\n');
}

fn build_transition(name: &str, guard: &str, effect: &str, states: &[String]) -> Transition {
    let has_parameters = name.contains('(')
        || guard.contains("\\E reason \\in")
        || guard.contains("\\E item \\in")
        || effect.contains("reason'")
        || effect.contains("return_reason'");

    Transition {
        name: name.split('(').next().unwrap_or(name).to_string(),
        from_states: extract_from_states(guard, states),
        to_state: extract_to_state(effect, states),
        guard_expr: guard.trim().to_string(),
        has_parameters,
        effect_expr: effect.trim().to_string(),
    }
}

fn extract_from_states(guard: &str, states: &[String]) -> Vec<String> {
    let mut result = Vec::new();

    for line in guard.lines() {
        let trimmed = line.trim();

        if let Some(state) = extract_guard_state(trimmed, states)
            && !result.contains(&state)
        {
            result.push(state);
        }

        if trimmed.contains("status \\in") {
            for state in extract_inline_set(trimmed) {
                if states.contains(&state) && !result.contains(&state) {
                    result.push(state);
                }
            }
        }
    }

    result
}

fn extract_guard_state(line: &str, states: &[String]) -> Option<String> {
    if line.contains("status =")
        && !line.contains("status' =")
        && let Some(state) = extract_quoted_string(line)
        && states.contains(&state)
    {
        return Some(state);
    }

    None
}

fn extract_to_state(effect: &str, states: &[String]) -> Option<String> {
    for line in effect.lines() {
        let trimmed = line.trim();
        if trimmed.contains("status'")
            && trimmed.contains('=')
            && let Some(state) = extract_quoted_string(trimmed)
            && states.contains(&state)
        {
            return Some(state);
        }
    }

    None
}
