//! GEPA Replay WASM module.
//!
//! Replays full OTS trajectories as workflows against a candidate IOA spec,
//! while preserving backward compatibility with flat `TrajectoryActions` input.

use temper_wasm_sdk::prelude::*;

temper_module! {
    fn run(ctx: Context) -> Result<Value> {
        ctx.log("info", "gepa-replay: starting workflow replay");

        let fields = ctx.entity_state.get("fields").unwrap_or(&ctx.entity_state);
        let ioa_source = fields
            .get("SpecSource")
            .and_then(Value::as_str)
            .or_else(|| ctx.trigger_params.get("SpecSource").and_then(Value::as_str))
            .ok_or("entity_state.fields missing 'SpecSource'")?;

        let inferred_initial_state = parse_initial_state_from_ioa(ioa_source);
        let initial_state = ctx
            .trigger_params
            .get("InitialState")
            .and_then(Value::as_str)
            .or_else(|| ctx.trigger_params.get("initial_state").and_then(Value::as_str))
            .or(inferred_initial_state.as_deref())
            .unwrap_or("Created");

        let trajectories = read_trajectories(&ctx, fields)?;

        let mut workflows: Vec<Value> = Vec::new();
        let mut all_errors: Vec<Value> = Vec::new();
        let mut all_action_results: Vec<Value> = Vec::new();
        let mut per_action = serde_json::Map::<String, Value>::new();

        let mut actions_attempted: u32 = 0;
        let mut succeeded: u32 = 0;
        let mut guard_rejections: u32 = 0;
        let mut unknown_actions: u32 = 0;
        let mut invalid_transitions: u32 = 0;

        let mut workflows_completed: u32 = 0;
        let mut workflows_partial: u32 = 0;
        let mut workflows_failed: u32 = 0;
        let mut workflows_empty: u32 = 0;

        for (trajectory_index, trajectory) in trajectories.iter().enumerate() {
            let metadata = trajectory.get("metadata").unwrap_or(trajectory);
            let trajectory_id = trajectory
                .get("trajectory_id")
                .and_then(Value::as_str)
                .or_else(|| metadata.get("trajectory_id").and_then(Value::as_str))
                .or_else(|| trajectory.get("id").and_then(Value::as_str))
                .or_else(|| metadata.get("id").and_then(Value::as_str))
                .map(str::to_string)
                .unwrap_or_else(|| format!("trajectory-{trajectory_index}"));
            let agent_goal = metadata
                .get("goal")
                .or_else(|| metadata.get("outcome"))
                .or_else(|| metadata.get("task"))
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();

            let mut workflow_current_state = initial_state.to_string();
            let mut workflow_attempted: u32 = 0;
            let mut workflow_succeeded: u32 = 0;
            let mut workflow_errors: Vec<Value> = Vec::new();
            let mut workflow_action_results: Vec<Value> = Vec::new();
            let mut workflow_actions_sequence: Vec<String> = Vec::new();
            let mut breakdown: Option<Value> = None;
            let mut reasoning_snippets: Vec<String> = Vec::new();

            let turns = trajectory
                .get("turns")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();

            for (turn_index, turn) in turns.iter().enumerate() {
                let extracted_actions = extract_actions_from_turn(turn);
                let turn_reasoning = extract_reasoning_from_turn(turn);
                if !turn_reasoning.is_empty() {
                    reasoning_snippets.push(format!("turn {}: {}", turn_index + 1, turn_reasoning));
                }

                for action_val in extracted_actions {
                    let Some(normalized) = normalize_trajectory_action(&action_val) else {
                        continue;
                    };

                    let action = normalized
                        .get("action")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_string();
                    let params = normalized
                        .get("params")
                        .cloned()
                        .unwrap_or_else(|| json!({}));
                    let params_str = params.to_string();
                    let from_state = workflow_current_state.clone();

                    workflow_attempted += 1;
                    actions_attempted += 1;
                    workflow_actions_sequence.push(action.clone());

                    let eval_result =
                        ctx.evaluate_spec(ioa_source, &workflow_current_state, &action, &params_str)?;
                    let success = eval_result
                        .get("success")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let error_message = eval_result
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let error_kind = classify_error(&error_message);

                    let to_state = if success {
                        eval_result
                            .get("new_state")
                            .and_then(Value::as_str)
                            .unwrap_or(&from_state)
                            .to_string()
                    } else {
                        from_state.clone()
                    };

                    if success {
                        workflow_succeeded += 1;
                        succeeded += 1;
                        workflow_current_state = to_state.clone();
                    } else {
                        match error_kind {
                            "unknown_action" => unknown_actions += 1,
                            "guard_rejection" => guard_rejections += 1,
                            _ => invalid_transitions += 1,
                        }

                        let err = json!({
                            "trajectory_id": trajectory_id,
                            "turn_index": turn_index,
                            "action": action,
                            "from_state": from_state,
                            "error_kind": error_kind,
                            "message": if error_message.is_empty() { "spec evaluation failed" } else { &error_message },
                        });
                        workflow_errors.push(err.clone());
                        all_errors.push(err.clone());
                        if breakdown.is_none() {
                            breakdown = Some(err);
                        }
                    }

                    let stats_entry = per_action
                        .entry(action.clone())
                        .or_insert_with(|| {
                            json!({
                                "attempted": 0_u64,
                                "succeeded": 0_u64,
                                "guard_rejections": 0_u64,
                                "unknown_actions": 0_u64,
                                "invalid_transitions": 0_u64,
                            })
                        });
                    if let Some(obj) = stats_entry.as_object_mut() {
                        let attempted = obj.get("attempted").and_then(Value::as_u64).unwrap_or(0);
                        obj.insert("attempted".into(), json!(attempted + 1));
                        if success {
                            let succ = obj.get("succeeded").and_then(Value::as_u64).unwrap_or(0);
                            obj.insert("succeeded".into(), json!(succ + 1));
                        } else {
                            match error_kind {
                                "guard_rejection" => {
                                    let n = obj
                                        .get("guard_rejections")
                                        .and_then(Value::as_u64)
                                        .unwrap_or(0);
                                    obj.insert("guard_rejections".into(), json!(n + 1));
                                }
                                "unknown_action" => {
                                    let n = obj
                                        .get("unknown_actions")
                                        .and_then(Value::as_u64)
                                        .unwrap_or(0);
                                    obj.insert("unknown_actions".into(), json!(n + 1));
                                }
                                _ => {
                                    let n = obj
                                        .get("invalid_transitions")
                                        .and_then(Value::as_u64)
                                        .unwrap_or(0);
                                    obj.insert("invalid_transitions".into(), json!(n + 1));
                                }
                            }
                        }
                    }

                    let action_result = json!({
                        "trajectory_id": trajectory_id,
                        "turn_index": turn_index,
                        "action": action,
                        "params": params,
                        "from_state": from_state,
                        "to_state": to_state,
                        "success": success,
                        "error_kind": if success { Value::Null } else { json!(error_kind) },
                        "error": if error_message.is_empty() { Value::Null } else { json!(error_message) },
                    });
                    workflow_action_results.push(action_result.clone());
                    all_action_results.push(action_result);
                }
            }

            let outcome = if workflow_attempted == 0 {
                workflows_empty += 1;
                "empty"
            } else if workflow_errors.is_empty() {
                workflows_completed += 1;
                "completed"
            } else if workflow_succeeded > 0 {
                workflows_partial += 1;
                "partial"
            } else {
                workflows_failed += 1;
                "failed"
            };

            workflows.push(json!({
                "trajectory_id": trajectory_id,
                "agent_goal": agent_goal,
                "outcome": outcome,
                "actions_attempted": workflow_attempted,
                "actions_total": workflow_attempted,
                "actions_succeeded": workflow_succeeded,
                "final_state": workflow_current_state,
                "breakdown_point": breakdown,
                "breakdown": breakdown,
                "errors": workflow_errors,
                "action_results": workflow_action_results,
                "action_sequence": workflow_actions_sequence,
                "reasoning_chain": if reasoning_snippets.is_empty() {
                    Value::Null
                } else {
                    json!(reasoning_snippets.join("\n"))
                },
            }));
        }

        let workflows_attempted = workflows_completed + workflows_partial + workflows_failed;

        let workflow_completion_rate = if workflows_attempted > 0 {
            workflows_completed as f64 / workflows_attempted as f64
        } else {
            0.0
        };
        let partial_adjusted_rate = if workflows_attempted > 0 {
            (workflows_completed as f64 + 0.5 * workflows_partial as f64) / workflows_attempted as f64
        } else {
            0.0
        };

        let success_rate = if actions_attempted > 0 {
            succeeded as f64 / actions_attempted as f64
        } else {
            0.0
        };
        let guard_pass_rate = if actions_attempted > 0 {
            1.0 - (guard_rejections as f64 / actions_attempted as f64)
        } else {
            0.0
        };
        let coverage = if actions_attempted > 0 {
            1.0 - (unknown_actions as f64 / actions_attempted as f64)
        } else {
            0.0
        };
        let transition_validity = if actions_attempted > 0 {
            1.0 - (invalid_transitions as f64 / actions_attempted as f64)
        } else {
            0.0
        };

        let replay_result = json!({
            // Workflow-level metrics
            "workflows_total": workflows.len(),
            "workflows_attempted": workflows_attempted,
            "workflows_completed": workflows_completed,
            "workflows_partial": workflows_partial,
            "workflows_failed": workflows_failed,
            "workflows_empty": workflows_empty,
            "workflow_completion_rate": workflow_completion_rate,
            "partial_adjusted_rate": partial_adjusted_rate,
            "workflows": workflows,

            // Aggregated action-level metrics
            "actions_attempted": actions_attempted,
            "succeeded": succeeded,
            "guard_rejections": guard_rejections,
            "unknown_actions": unknown_actions,
            "invalid_transitions": invalid_transitions,
            "success_rate": success_rate,
            "guard_pass_rate": guard_pass_rate,
            "coverage": coverage,
            "transition_validity": transition_validity,
            "action_stats": {
                "attempted": actions_attempted,
                "succeeded": succeeded,
                "guard_rejections": guard_rejections,
                "unknown_actions": unknown_actions,
                "invalid_transitions": invalid_transitions,
                "success_rate": success_rate,
                "guard_pass_rate": guard_pass_rate,
                "coverage": coverage,
                "transition_validity": transition_validity,
            },

            // Detailed traces
            "errors": all_errors,
            "action_results": all_action_results,
            "per_action": Value::Object(per_action),
        });

        ctx.log(
            "info",
            &format!(
                "gepa-replay: workflows completed={workflows_completed}/{workflows_attempted}, actions succeeded={succeeded}/{actions_attempted}"
            ),
        );

        Ok(json!({
            "ReplayResultJson": replay_result.to_string(),
            "replay_result": replay_result,
        }))
    }
}

fn read_trajectories(ctx: &Context, fields: &Value) -> std::result::Result<Vec<Value>, String> {
    if let Some(value) = ctx
        .trigger_params
        .get("Trajectories")
        .or_else(|| fields.get("Trajectories"))
    {
        let parsed = parse_trajectories_value(value);
        if !parsed.is_empty() {
            return Ok(parsed);
        }
    }

    if let Some(value) = ctx
        .trigger_params
        .get("TrajectoryActions")
        .or_else(|| fields.get("TrajectoryActions"))
    {
        let actions = parse_actions_value(value);
        if !actions.is_empty() {
            return Ok(vec![wrap_flat_actions_as_trajectory(actions)]);
        }
    }

    Err("trigger_params missing 'Trajectories' or 'TrajectoryActions'".into())
}

fn parse_trajectories_value(value: &Value) -> Vec<Value> {
    match value {
        Value::Array(arr) => arr.clone(),
        Value::String(raw) => {
            if let Ok(parsed) = serde_json::from_str::<Value>(raw) {
                match parsed {
                    Value::Array(arr) => arr,
                    Value::Object(_) => vec![parsed],
                    _ => Vec::new(),
                }
            } else {
                Vec::new()
            }
        }
        Value::Object(_) => vec![value.clone()],
        _ => Vec::new(),
    }
}

fn parse_actions_value(value: &Value) -> Vec<Value> {
    match value {
        Value::Array(arr) => arr.clone(),
        Value::String(raw) => serde_json::from_str::<Vec<Value>>(raw).unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn wrap_flat_actions_as_trajectory(actions: Vec<Value>) -> Value {
    let synthetic_turns: Vec<Value> = actions
        .into_iter()
        .map(|raw| {
            let normalized = normalize_trajectory_action(&raw).unwrap_or_else(|| {
                json!({
                    "action": "unknown",
                    "params": {},
                })
            });
            json!({
                "decisions": [{
                    "choice": {
                        "action": normalized.get("action").and_then(Value::as_str).unwrap_or("unknown"),
                        "arguments": normalized.get("params").cloned().unwrap_or_else(|| json!({})),
                    }
                }]
            })
        })
        .collect();

    json!({
        "metadata": {
            "trajectory_id": "legacy-flat",
            "goal": "legacy-flat-actions"
        },
        "turns": synthetic_turns,
    })
}

fn extract_actions_from_turn(turn: &Value) -> Vec<Value> {
    let mut actions = Vec::new();

    if let Some(decisions) = turn.get("decisions").and_then(Value::as_array) {
        for decision in decisions {
            if let Some(raw_actions) = decision
                .get("choice")
                .and_then(|choice| choice.get("arguments"))
                .and_then(|args| args.get("trajectory_actions"))
                .and_then(Value::as_array)
            {
                for raw in raw_actions {
                    actions.push(raw.clone());
                }
                continue;
            }

            let action_name = decision
                .get("choice")
                .and_then(|choice| choice.get("action"))
                .and_then(Value::as_str)
                .or_else(|| decision.get("action").and_then(Value::as_str));

            if let Some(action) = action_name {
                if action.starts_with("execute:") {
                    continue;
                }
                let params = decision
                    .get("choice")
                    .and_then(|choice| choice.get("arguments"))
                    .or_else(|| decision.get("params"))
                    .and_then(parse_params_value)
                    .unwrap_or_else(|| json!({}));
                actions.push(json!({
                    "action": action,
                    "params": params,
                }));
            }
        }
    }

    if actions.is_empty()
        && let Some(messages) = turn.get("messages").and_then(Value::as_array)
    {
        for message in messages {
            let role = message
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if role != "user" {
                continue;
            }
            if let Some(code) = extract_message_text(message) {
                actions.extend(extract_temper_actions_from_code(&code));
            }
        }
    }

    actions
}

fn extract_reasoning_from_turn(turn: &Value) -> String {
    let mut parts = Vec::new();

    if let Some(decisions) = turn.get("decisions").and_then(Value::as_array) {
        for decision in decisions {
            if let Some(reasoning) = decision
                .get("reasoning")
                .and_then(Value::as_str)
                .or_else(|| {
                    decision
                        .get("choice")
                        .and_then(|choice| choice.get("rationale"))
                        .and_then(Value::as_str)
                })
                && !reasoning.trim().is_empty()
            {
                parts.push(reasoning.trim().to_string());
            }
        }
    }

    if let Some(messages) = turn.get("messages").and_then(Value::as_array) {
        for message in messages {
            let role = message
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if role != "assistant" {
                continue;
            }

            if let Some(reasoning) = message.get("reasoning").and_then(Value::as_str)
                && !reasoning.trim().is_empty()
            {
                parts.push(reasoning.trim().to_string());
            }

            if let Some(text) = extract_message_text(message)
                && !text.trim().is_empty()
            {
                let trimmed = text.trim();
                let clipped = if trimmed.len() > 320 {
                    &trimmed[..320]
                } else {
                    trimmed
                };
                parts.push(clipped.to_string());
            }
        }
    }

    parts.join(" | ")
}

fn extract_message_text(message: &Value) -> Option<String> {
    if let Some(text) = message
        .get("content")
        .and_then(|content| content.get("text"))
        .and_then(Value::as_str)
    {
        return Some(text.to_string());
    }

    message
        .get("content")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn classify_error(error_message: &str) -> &'static str {
    let lowered = error_message.to_ascii_lowercase();
    if lowered.contains("unknown action") || lowered.contains("not defined") {
        "unknown_action"
    } else if lowered.contains("guard") {
        "guard_rejection"
    } else {
        "invalid_transition"
    }
}

fn normalize_trajectory_action(raw: &Value) -> Option<Value> {
    match raw {
        Value::String(action_name) => Some(json!({
            "action": action_name,
            "params": {},
        })),
        Value::Object(obj) => {
            let action = obj
                .get("action")
                .or_else(|| obj.get("Action"))
                .and_then(Value::as_str)?;
            let params = obj
                .get("params")
                .or_else(|| obj.get("Params"))
                .and_then(parse_params_value)
                .unwrap_or_else(|| json!({}));
            Some(json!({
                "action": action,
                "params": params,
            }))
        }
        _ => None,
    }
}

fn parse_params_value(value: &Value) -> Option<Value> {
    match value {
        Value::Object(_) => Some(value.clone()),
        Value::Null => Some(json!({})),
        Value::String(s) => {
            if let Ok(parsed) = serde_json::from_str::<Value>(s) {
                return Some(parsed);
            }
            Some(json!({}))
        }
        _ => Some(json!({})),
    }
}

fn parse_initial_state_from_ioa(ioa_source: &str) -> Option<String> {
    let mut in_automaton = false;

    for raw_line in ioa_source.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if line.starts_with('[') && line.ends_with(']') {
            in_automaton = line == "[automaton]";
            continue;
        }

        if !in_automaton {
            continue;
        }

        if line.starts_with("initial") {
            if let Some((_, rhs)) = line.split_once('=') {
                let value = rhs.trim().trim_matches('"').trim_matches('\'').trim();
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }

    None
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
                .unwrap_or_else(|| json!({}));
            actions.push(json!({
                "action": action_name,
                "params": params,
            }));
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
        return Some(json!({}));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_initial_state_from_ioa_reads_automaton_initial() {
        let ioa = r#"
[automaton]
name = "Issue"
states = ["Backlog", "Done"]
initial = "Backlog"
"#;

        assert_eq!(
            parse_initial_state_from_ioa(ioa).as_deref(),
            Some("Backlog")
        );
    }

    #[test]
    fn extract_actions_from_turn_skips_execute_choice_without_trajectory_actions() {
        let turn = json!({
            "decisions": [{
                "choice": {
                    "action": "execute: await temper.flush_trajectory()",
                    "arguments": {}
                }
            }]
        });

        let actions = extract_actions_from_turn(&turn);
        assert!(actions.is_empty(), "execute pseudo-actions should be ignored");
    }

    #[test]
    fn extract_actions_from_turn_uses_embedded_trajectory_actions() {
        let turn = json!({
            "decisions": [{
                "choice": {
                    "action": "execute: ...",
                    "arguments": {
                        "trajectory_actions": [
                            { "action": "Assign", "params": { "AgentId": "a1" } },
                            { "action": "Reassign", "params": { "NewAssigneeId": "a2" } }
                        ]
                    }
                }
            }]
        });

        let actions = extract_actions_from_turn(&turn);
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0].get("action").and_then(Value::as_str), Some("Assign"));
        assert_eq!(
            actions[1].get("action").and_then(Value::as_str),
            Some("Reassign")
        );
    }
}
