use serde_json::Value;

pub(super) fn has_replay_trajectory_input(params: &Value) -> bool {
    has_non_empty_param(params, "Trajectories") || has_non_empty_param(params, "TrajectoryActions")
}

pub(super) fn extract_trajectory_actions_from_ots(trajectory: &Value) -> Vec<Value> {
    let mut actions = Vec::new();

    let Some(turns) = trajectory.get("turns").and_then(Value::as_array) else {
        return actions;
    };

    for turn in turns {
        if let Some(decisions) = turn.get("decisions").and_then(Value::as_array) {
            for decision in decisions {
                if let Some(raw_actions) = decision
                    .get("choice")
                    .and_then(|choice| choice.get("arguments"))
                    .and_then(|args| args.get("trajectory_actions"))
                    .and_then(Value::as_array)
                {
                    for raw in raw_actions {
                        if let Some(normalized) = normalize_trajectory_action(raw) {
                            actions.push(normalized);
                        }
                    }
                }

                if let Some(choice_action) = decision
                    .get("choice")
                    .and_then(|choice| choice.get("action"))
                    .and_then(Value::as_str)
                    && let Some(code) = choice_action.strip_prefix("execute:")
                {
                    actions.extend(extract_temper_actions_from_code(code));
                }
            }
        }

        if let Some(messages) = turn.get("messages").and_then(Value::as_array) {
            for message in messages {
                let role = message
                    .get("role")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if role != "user" {
                    continue;
                }
                let text = message
                    .get("content")
                    .and_then(|content| content.get("text"))
                    .and_then(Value::as_str);
                if let Some(code) = text {
                    actions.extend(extract_temper_actions_from_code(code));
                }
            }
        }
    }

    dedupe_actions(actions)
}

fn has_non_empty_param(params: &Value, key: &str) -> bool {
    match params.get(key) {
        Some(Value::Array(arr)) => !arr.is_empty(),
        Some(Value::String(s)) => !s.trim().is_empty(),
        Some(Value::Object(obj)) => !obj.is_empty(),
        Some(_) => true,
        None => false,
    }
}

fn normalize_trajectory_action(raw: &Value) -> Option<Value> {
    match raw {
        Value::String(action_name) => Some(action_value(action_name, serde_json::json!({}))),
        Value::Object(obj) => {
            let action = obj
                .get("action")
                .or_else(|| obj.get("Action"))
                .and_then(Value::as_str)?;

            let params = obj
                .get("params")
                .or_else(|| obj.get("Params"))
                .and_then(parse_params_value)
                .unwrap_or_else(|| serde_json::json!({}));

            Some(action_value(action, params))
        }
        _ => None,
    }
}

fn parse_params_value(value: &Value) -> Option<Value> {
    match value {
        Value::Object(_) => Some(value.clone()),
        Value::Null => Some(serde_json::json!({})),
        Value::String(s) => serde_json::from_str::<Value>(s)
            .ok()
            .or_else(|| Some(serde_json::json!({}))),
        _ => Some(serde_json::json!({})),
    }
}

fn dedupe_actions(actions: Vec<Value>) -> Vec<Value> {
    let mut deduped = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for action in actions {
        let key = action.to_string();
        if seen.insert(key) {
            deduped.push(action);
        }
    }
    deduped
}

fn extract_temper_actions_from_code(code: &str) -> Vec<Value> {
    let mut actions = Vec::new();
    let mut cursor = 0usize;
    let needle = "temper.action";

    while let Some(found) = code[cursor..].find(needle) {
        let method_start = cursor + found + needle.len();
        let mut open = method_start;
        while open < code.len()
            && code
                .as_bytes()
                .get(open)
                .is_some_and(|b| b.is_ascii_whitespace())
        {
            open += 1;
        }
        if code.as_bytes().get(open) != Some(&b'(') {
            cursor = method_start;
            continue;
        }
        let Some(close) = find_matching_paren(code, open) else {
            break;
        };

        let args = split_top_level_args(&code[open + 1..close]);
        let (action_idx, params_idx) =
            if args.len() >= 5 && parse_python_string_literal(args[3]).is_some() {
                (3usize, 4usize)
            } else {
                (2usize, 3usize)
            };

        if args.len() > action_idx
            && let Some(action_name) = parse_python_string_literal(args[action_idx])
        {
            let params = args
                .get(params_idx)
                .and_then(|raw| parse_python_json_value(raw))
                .unwrap_or_else(|| serde_json::json!({}));
            actions.push(action_value(&action_name, params));
        }

        cursor = close + 1;
    }

    actions
}

fn find_matching_paren(input: &str, open_idx: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_quote: Option<char> = None;
    let mut escaped = false;

    for (offset, ch) in input[open_idx..].char_indices() {
        let idx = open_idx + offset;
        if let Some(quote) = in_quote {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if ch == quote {
                in_quote = None;
            }
            continue;
        }

        match ch {
            '\'' | '"' => in_quote = Some(ch),
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(idx);
                }
            }
            _ => {}
        }
    }
    None
}

fn split_top_level_args(input: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut depth_paren = 0i32;
    let mut depth_brace = 0i32;
    let mut depth_bracket = 0i32;
    let mut in_quote: Option<char> = None;
    let mut escaped = false;

    for (idx, ch) in input.char_indices() {
        if let Some(quote) = in_quote {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if ch == quote {
                in_quote = None;
            }
            continue;
        }

        match ch {
            '\'' | '"' => in_quote = Some(ch),
            '(' => depth_paren += 1,
            ')' => depth_paren -= 1,
            '{' => depth_brace += 1,
            '}' => depth_brace -= 1,
            '[' => depth_bracket += 1,
            ']' => depth_bracket -= 1,
            ',' if depth_paren == 0 && depth_brace == 0 && depth_bracket == 0 => {
                parts.push(input[start..idx].trim());
                start = idx + 1;
            }
            _ => {}
        }
    }

    if start <= input.len() {
        let tail = input[start..].trim();
        if !tail.is_empty() {
            parts.push(tail);
        }
    }
    parts
}

fn parse_python_string_literal(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.len() < 2 {
        return None;
    }
    let quote = s.chars().next()?;
    if (quote != '\'' && quote != '"') || !s.ends_with(quote) {
        return None;
    }

    let mut out = String::new();
    let mut escaped = false;
    for ch in s[1..s.len() - 1].chars() {
        if escaped {
            let mapped = match ch {
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                '\\' => '\\',
                '\'' => '\'',
                '"' => '"',
                other => other,
            };
            out.push(mapped);
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        out.push(ch);
    }
    if escaped {
        out.push('\\');
    }
    Some(out)
}

fn parse_python_json_value(raw: &str) -> Option<Value> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Some(serde_json::json!({}));
    }
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        return Some(v);
    }
    let normalized = normalize_pythonish_json(trimmed);
    serde_json::from_str::<Value>(&normalized).ok()
}

fn normalize_pythonish_json(input: &str) -> String {
    let mut quoted = String::with_capacity(input.len());
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;

    for ch in input.chars() {
        if in_single {
            if escaped {
                quoted.push(ch);
                escaped = false;
                continue;
            }
            match ch {
                '\\' => escaped = true,
                '\'' => {
                    in_single = false;
                    quoted.push('"');
                }
                '"' => quoted.push_str("\\\""),
                _ => quoted.push(ch),
            }
            continue;
        }

        if in_double {
            quoted.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_double = false;
            }
            continue;
        }

        match ch {
            '\'' => {
                in_single = true;
                quoted.push('"');
            }
            '"' => {
                in_double = true;
                quoted.push('"');
            }
            _ => quoted.push(ch),
        }
    }

    let mut out = String::with_capacity(quoted.len());
    let mut token = String::new();
    let mut in_string = false;
    let mut esc = false;

    let flush_token = |token: &mut String, out: &mut String| {
        if token.is_empty() {
            return;
        }
        match token.as_str() {
            "True" => out.push_str("true"),
            "False" => out.push_str("false"),
            "None" => out.push_str("null"),
            _ => out.push_str(token),
        }
        token.clear();
    };

    for ch in quoted.chars() {
        if in_string {
            out.push(ch);
            if esc {
                esc = false;
            } else if ch == '\\' {
                esc = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        if ch == '"' {
            flush_token(&mut token, &mut out);
            in_string = true;
            out.push(ch);
            continue;
        }

        if ch.is_ascii_alphanumeric() || ch == '_' {
            token.push(ch);
            continue;
        }

        flush_token(&mut token, &mut out);
        out.push(ch);
    }
    flush_token(&mut token, &mut out);

    out
}

fn action_value(action: &str, params: Value) -> Value {
    serde_json::json!({
        "action": action,
        "params": params,
    })
}

#[cfg(test)]
#[path = "replay_inputs_test.rs"]
mod tests;
