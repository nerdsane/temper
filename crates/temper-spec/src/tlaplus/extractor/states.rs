use super::TlaExtractError;
use super::source::{extract_inline_set, extract_quoted_string};

pub(super) fn extract_states(source: &str) -> Result<Vec<String>, TlaExtractError> {
    let mut states = extract_declared_states(source);
    if states.is_empty() {
        states = extract_fallback_states(source);
    }

    if states.is_empty() {
        return Err(TlaExtractError::NoStates);
    }

    Ok(states)
}

fn extract_declared_states(source: &str) -> Vec<String> {
    for line in source.lines() {
        let trimmed = line.trim();
        if (trimmed.contains("Statuses ==") || trimmed.contains("States =="))
            && trimmed.contains('{')
        {
            return extract_string_set(source, trimmed);
        }
    }

    Vec::new()
}

fn extract_fallback_states(source: &str) -> Vec<String> {
    let mut states = Vec::new();

    for line in source.lines() {
        let trimmed = line.trim();
        if let Some(state) = extract_status_assignment(trimmed)
            && !states.contains(&state)
        {
            states.push(state);
        }

        if trimmed.contains("status \\in") {
            for state in extract_inline_set(trimmed) {
                if !states.contains(&state) {
                    states.push(state);
                }
            }
        }
    }

    states
}

fn extract_string_set(source: &str, start_line: &str) -> Vec<String> {
    let mut buffer = String::new();
    let mut collecting = false;

    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed == start_line || (collecting && !buffer.contains('}')) {
            collecting = true;
            buffer.push_str(trimmed);
            buffer.push(' ');
        }
        if collecting && buffer.contains('}') {
            break;
        }
    }

    extract_inline_set(&buffer)
}

fn extract_status_assignment(line: &str) -> Option<String> {
    if (line.contains("status =") || line.contains("status' ="))
        && let Some(state) = extract_quoted_string(line)
    {
        return Some(state);
    }

    None
}
