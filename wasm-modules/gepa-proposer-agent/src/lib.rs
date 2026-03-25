//! GEPA mutation proposer WASM module driven by TemperAgent entities.
//!
//! This module replaces direct local-CLI adapters in the evolution pipeline.
//! It orchestrates a `TemperAgent` run through Temper's own entity actions:
//! create -> configure -> provision -> poll -> extract mutation JSON.

use temper_wasm_sdk::prelude::*;

temper_module! {
    fn run(ctx: Context) -> Result<Value> {
        ctx.log("info", "gepa-proposer-agent: starting TemperAgent-driven mutation proposal");

        let fields = ctx.entity_state.get("fields").unwrap_or(&ctx.entity_state);
        let dataset_json = read_dataset_json(&ctx, fields)?;
        let spec_source = fields
            .get("SpecSource")
            .and_then(Value::as_str)
            .or_else(|| ctx.trigger_params.get("SpecSource").and_then(Value::as_str))
            .ok_or("missing SpecSource in EvolutionRun state/trigger params")?;

        let dataset_missing_capabilities = extract_dataset_missing_capabilities(&dataset_json);

        let skill_name = fields
            .get("SkillName")
            .and_then(Value::as_str)
            .unwrap_or("unknown-skill");
        let entity_type = fields
            .get("TargetEntityType")
            .and_then(Value::as_str)
            .unwrap_or("unknown-entity");
        let evo_id = fields
            .get("Id")
            .and_then(Value::as_str)
            .unwrap_or("evolution-run");
        let candidate_id = fields
            .get("CandidateId")
            .and_then(Value::as_str)
            .or_else(|| ctx.trigger_params.get("CandidateId").and_then(Value::as_str))
            .unwrap_or("candidate");
        let attempt = fields
            .get("mutation_attempts")
            .and_then(Value::as_i64)
            .or_else(|| {
                fields
                    .get("mutation_attempts")
                    .and_then(Value::as_str)
                    .and_then(|s| s.parse::<i64>().ok())
            })
            .unwrap_or(0);

        let base_url = ctx
            .config
            .get("temper_api_url")
            .cloned()
            .unwrap_or_else(|| "http://127.0.0.1:3000".to_string());
        let sandbox_url = ctx
            .config
            .get("sandbox_url")
            .cloned()
            .unwrap_or_else(|| "http://127.0.0.1:9999".to_string());
        let model = ctx
            .config
            .get("model")
            .cloned()
            .unwrap_or_else(|| "claude-sonnet-4-20250514".to_string());
        let provider = ctx
            .config
            .get("provider")
            .cloned()
            .unwrap_or_else(|| "anthropic".to_string());
        let max_turns = ctx
            .config
            .get("max_turns")
            .cloned()
            .unwrap_or_else(|| "10".to_string());
        let workdir = ctx
            .config
            .get("workdir")
            .cloned()
            .unwrap_or_else(|| "/tmp/workspace".to_string());
        let tools_enabled = ctx
            .config
            .get("tools_enabled")
            .cloned()
            .unwrap_or_else(|| "read,write,edit,bash".to_string());
        let poll_attempts = ctx
            .config
            .get("poll_attempts")
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(240);
        let poll_sleep_ms = ctx
            .config
            .get("poll_sleep_ms")
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(250);

        let max_agent_retries = ctx
            .config
            .get("max_agent_retries")
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(3)
            .max(1);

        let mut headers = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("X-Tenant-Id".to_string(), ctx.tenant.clone()),
            // Drive TemperAgent via Cedar-governed agent identity.
            ("x-temper-principal-kind".to_string(), "agent".to_string()),
            (
                "x-temper-principal-id".to_string(),
                "gepa-proposer-agent".to_string(),
            ),
            ("x-temper-agent-type".to_string(), "supervisor".to_string()),
        ];
        let api_key = ctx
            .config
            .get("temper_api_key")
            .cloned()
            .or_else(|| ctx.config.get("api_key").cloned())
            .or_else(|| ctx.config.get("bearer_token").cloned())
            .filter(|s| !s.trim().is_empty())
            .or_else(|| ctx.get_secret("temper_api_key").ok())
            .filter(|s| !s.trim().is_empty());
        if let Some(api_key) = api_key {
            headers.push((
                "Authorization".to_string(),
                format!("Bearer {}", api_key.trim()),
            ));
        }

        let system_prompt = ctx
            .config
            .get("system_prompt")
            .cloned()
            .unwrap_or_else(default_system_prompt);
        let base_user_message = build_user_message(skill_name, entity_type, spec_source, &dataset_json);
        let mut last_error = String::new();

        for agent_retry in 0..max_agent_retries {
            let agent_id = build_agent_id(evo_id, candidate_id, attempt, agent_retry);
            let create_url = format!("{base_url}/tdata/TemperAgents");
            let create_resp = post_json(
                &ctx,
                &create_url,
                &headers,
                json!({
                    "TemperAgentId": agent_id,
                }),
            )?;
            let created_agent_id = extract_entity_id(&create_resp).unwrap_or_else(|| {
                create_resp
                    .get("fields")
                    .and_then(|f| f.get("Id"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown-agent")
                    .to_string()
            });

            let user_message = if agent_retry == 0 {
                base_user_message.clone()
            } else {
                format!(
                    "{base_user_message}\n\nIMPORTANT: previous attempt returned empty/invalid payload. \
Return valid compact JSON in one line with non-empty MutatedSpecSource and MutationSummary."
                )
            };

            let cfg_url = format!(
                "{base_url}/tdata/TemperAgents('{created_agent_id}')/Temper.Agent.TemperAgent.Configure"
            );
            let _ = post_json(
                &ctx,
                &cfg_url,
                &headers,
                json!({
                    "system_prompt": system_prompt,
                    "user_message": user_message,
                    "model": model,
                    "provider": provider,
                    "max_turns": max_turns,
                    "tools_enabled": tools_enabled,
                    "workdir": workdir,
                    "sandbox_url": sandbox_url,
                }),
            )?;

            let provision_url = format!(
                "{base_url}/tdata/TemperAgents('{created_agent_id}')/Temper.Agent.TemperAgent.Provision"
            );
            let _ = post_json(&ctx, &provision_url, &headers, json!({}))?;

            let mut attempt_finished = false;
            for poll in 0..poll_attempts {
                if poll > 0 && poll_sleep_ms > 0 {
                    let _ = sleep_tick(&ctx, &sandbox_url, &workdir, poll_sleep_ms);
                }
                let get_url = format!("{base_url}/tdata/TemperAgents('{created_agent_id}')");
                let entity = get_json(&ctx, &get_url, &headers)?;
                let status = entity
                    .get("status")
                    .and_then(Value::as_str)
                    .or_else(|| {
                        entity
                            .get("fields")
                            .and_then(|f| f.get("Status"))
                            .and_then(Value::as_str)
                    })
                    .unwrap_or("Unknown");

                match status {
                    "Completed" => {
                        let result_text = entity
                            .get("fields")
                            .and_then(|f| f.get("result"))
                            .and_then(Value::as_str)
                            .or_else(|| {
                                entity
                                    .get("fields")
                                    .and_then(|f| f.get("Result"))
                                    .and_then(Value::as_str)
                            })
                            .unwrap_or_default();

                        match extract_mutation_payload(result_text) {
                            Ok(payload) => {
                                let gate = validate_optimizer_only_spec_mutation(
                                    spec_source,
                                    &payload.mutated_spec_source,
                                );
                                let unmet_handoff = collect_unmet_intent_handoff(
                                    &dataset_missing_capabilities,
                                    &payload.unmet_intent_suggestions,
                                    &gate,
                                );
                                if gate.allowed {
                                    let mut out = json!({
                                        "MutatedSpecSource": payload.mutated_spec_source,
                                        "MutationSummary": payload.mutation_summary,
                                        "ProposerType": "temper_agent",
                                        "ProposerAgentId": created_agent_id,
                                    });
                                    if !unmet_handoff.is_empty() {
                                        let report_reason =
                                            "GEPA detected unmet capabilities during optimizer mutation";
                                        let report_outcomes = report_unmet_intents(
                                            &ctx,
                                            &base_url,
                                            &headers,
                                            skill_name,
                                            entity_type,
                                            &unmet_handoff,
                                            report_reason,
                                        );
                                        out["UnmetIntentSuggestions"] = Value::Array(
                                            unmet_handoff
                                                .iter()
                                                .map(|s| Value::String(s.clone()))
                                                .collect(),
                                        );
                                        out["UnmetIntentHandoff"] = Value::Array(
                                            unmet_handoff
                                                .iter()
                                                .map(|s| Value::String(s.clone()))
                                                .collect(),
                                        );
                                        out["UnmetIntentReport"] = report_outcomes;
                                        out["HasUnmetIntentHandoff"] = Value::Bool(true);
                                    }
                                    return Ok(out);
                                }

                                let gate_reasons = gate.reasons();
                                let report_reason = format!(
                                    "GEPA optimizer-only gate blocked structural mutation: {}",
                                    gate_reasons.join("; ")
                                );
                                let report_outcomes = report_unmet_intents(
                                    &ctx,
                                    &base_url,
                                    &headers,
                                    skill_name,
                                    entity_type,
                                    &unmet_handoff,
                                    &report_reason,
                                );

                                let summary = format!(
                                    "Optimizer-only GEPA gate rejected structural mutation ({}). \
Forwarded {} unmet-intent handoff items; returning no-op mutation for GEPA.",
                                    gate_reasons.join("; "),
                                    unmet_handoff.len()
                                );
                                ctx.log("warn", &summary);
                                return Ok(json!({
                                    "MutatedSpecSource": spec_source,
                                    "MutationSummary": summary,
                                    "ProposerType": "temper_agent",
                                    "ProposerAgentId": created_agent_id,
                                    "RequiresUnmetIntentLoop": true,
                                    "UnmetIntentHandoff": unmet_handoff,
                                    "UnmetIntentReport": report_outcomes,
                                    "OptimizerOnlyGate": {
                                        "blocked": true,
                                        "reasons": gate_reasons,
                                    },
                                }));
                            }
                            Err(err) => {
                                last_error = format!(
                                    "TemperAgent completed with invalid payload on retry {agent_retry}: {err}"
                                );
                                ctx.log("warn", &last_error);
                                attempt_finished = true;
                                break;
                            }
                        }
                    }
                    "Failed" | "Cancelled" => {
                        let err = entity
                            .get("fields")
                            .and_then(|f| f.get("error_message"))
                            .and_then(Value::as_str)
                            .or_else(|| {
                                entity
                                    .get("fields")
                                    .and_then(|f| f.get("ErrorMessage"))
                                    .and_then(Value::as_str)
                            })
                            .unwrap_or("TemperAgent run failed");
                        last_error = format!("TemperAgent {status} on retry {agent_retry}: {err}");
                        ctx.log("warn", &last_error);
                        attempt_finished = true;
                        break;
                    }
                    _ => {}
                }
            }

            if !attempt_finished {
                last_error = format!(
                    "Timed out waiting for TemperAgent completion after {poll_attempts} polls on retry {agent_retry}"
                );
                ctx.log("warn", &last_error);
            }
        }

        if last_error.is_empty() {
            Err("GEPA proposer failed without explicit error".to_string())
        } else {
            Err(last_error)
        }
    }
}

fn read_dataset_json(ctx: &Context, fields: &Value) -> Result<String, String> {
    if let Some(s) = ctx
        .trigger_params
        .get("DatasetJson")
        .and_then(Value::as_str)
    {
        return Ok(s.to_string());
    }
    if let Some(v) = ctx.trigger_params.get("reflective_dataset") {
        return Ok(v.to_string());
    }
    if let Some(s) = fields.get("DatasetJson").and_then(Value::as_str) {
        return Ok(s.to_string());
    }
    if let Some(v) = fields.get("reflective_dataset") {
        return Ok(v.to_string());
    }
    Err("missing DatasetJson in trigger/state".to_string())
}

fn post_json(
    ctx: &Context,
    url: &str,
    headers: &[(String, String)],
    body: Value,
) -> Result<Value, String> {
    let resp = ctx.http_call("POST", url, headers, &body.to_string())?;
    if !(200..300).contains(&resp.status) {
        return Err(format!(
            "POST {url} failed: HTTP {} body={}",
            resp.status, resp.body
        ));
    }
    parse_json_body(&resp.body)
}

fn get_json(ctx: &Context, url: &str, headers: &[(String, String)]) -> Result<Value, String> {
    let resp = ctx.http_call("GET", url, headers, "")?;
    if !(200..300).contains(&resp.status) {
        return Err(format!(
            "GET {url} failed: HTTP {} body={}",
            resp.status, resp.body
        ));
    }
    parse_json_body(&resp.body)
}

fn parse_json_body(body: &str) -> Result<Value, String> {
    if body.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str::<Value>(body)
        .map_err(|e| format!("failed to parse HTTP JSON body: {e}; body={body}"))
}

fn extract_entity_id(value: &Value) -> Option<String> {
    value
        .get("entity_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            value
                .get("fields")
                .and_then(|f| f.get("Id"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
}

fn default_system_prompt() -> String {
    "You are the GEPA evolution agent operating inside TemperAgent. \
GEPA in this run is optimizer-only: never introduce or remove entities, states, or actions. \
Return only compact JSON with keys MutatedSpecSource and MutationSummary (optional UnmetIntentSuggestions). \
Do not include markdown fences. Do not ask for permissions. \
Do not edit files; reason over the provided spec text."
        .to_string()
}

fn build_user_message(
    skill_name: &str,
    entity_type: &str,
    spec_source: &str,
    dataset_json: &str,
) -> String {
    format!(
        "Target skill: {skill_name}\n\
Target entity: {entity_type}\n\n\
Current IOA spec:\n{spec_source}\n\n\
Reflective dataset JSON:\n{dataset_json}\n\n\
Task:\n\
1) Read workflow-level triplets. Each triplet has:\n\
   - input: goal + reasoning chain\n\
   - output: what happened\n\
   - feedback: specific fix suggestion\n\
   - score: 1.0 success, 0.5 partial, 0.0 failed\n\
   - preserve: true means this working pattern must not regress\n\
2) Propose the minimal IOA mutation that improves workflow completion while preserving successful patterns.\n\
3) Triplets with preserve=true MUST remain valid after mutation.\n\
4) For failed/partial workflows, apply the feedback suggestion exactly where possible.\n\
5) GEPA optimizer-only constraint: DO NOT add/remove/rename entities, states, or actions.\n\
6) If patterns.missing_capabilities indicates net-new capability is needed, list it in UnmetIntentSuggestions instead of adding it to the spec.\n\
7) Keep schema/invariants coherent and avoid unrelated changes.\n\
Output strict JSON only:\n\
{{\"MutatedSpecSource\":\"...full spec...\",\"MutationSummary\":\"...\",\"UnmetIntentSuggestions\":[\"...\"]}}"
    )
}

fn sanitize_id(raw: &str) -> String {
    let mut out = String::new();
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else {
            out.push('-');
        }
    }
    if out.is_empty() {
        "id".to_string()
    } else {
        out.chars().take(48).collect()
    }
}

fn build_agent_id(
    evo_id: &str,
    candidate_id: &str,
    mutation_attempt: i64,
    agent_retry: usize,
) -> String {
    let base = format!(
        "evo-{}-{}-a{}-r{}",
        sanitize_id(evo_id),
        sanitize_id(candidate_id),
        mutation_attempt,
        agent_retry
    );
    if base.len() <= 96 {
        return base;
    }
    base.chars().take(96).collect()
}

#[derive(Debug, Clone)]
struct MutationPayload {
    mutated_spec_source: String,
    mutation_summary: String,
    unmet_intent_suggestions: Vec<String>,
}

#[derive(Debug, Clone)]
struct SpecShape {
    automaton_name: Option<String>,
    states: std::collections::BTreeSet<String>,
    actions: std::collections::BTreeSet<String>,
}

#[derive(Debug, Clone, Default)]
struct SpecShapeDelta {
    added_states: Vec<String>,
    removed_states: Vec<String>,
    added_actions: Vec<String>,
    removed_actions: Vec<String>,
    from_automaton_name: Option<String>,
    to_automaton_name: Option<String>,
}

#[derive(Debug, Clone)]
struct OptimizerOnlyGate {
    allowed: bool,
    delta: SpecShapeDelta,
}

impl OptimizerOnlyGate {
    fn reasons(&self) -> Vec<String> {
        let mut reasons = Vec::new();
        if self.delta.from_automaton_name != self.delta.to_automaton_name {
            reasons.push(format!(
                "entity changed from {:?} to {:?}",
                self.delta.from_automaton_name, self.delta.to_automaton_name
            ));
        }
        if !self.delta.added_states.is_empty() {
            reasons.push(format!(
                "added states: {}",
                self.delta.added_states.join(", ")
            ));
        }
        if !self.delta.removed_states.is_empty() {
            reasons.push(format!(
                "removed states: {}",
                self.delta.removed_states.join(", ")
            ));
        }
        if !self.delta.added_actions.is_empty() {
            reasons.push(format!(
                "added actions: {}",
                self.delta.added_actions.join(", ")
            ));
        }
        if !self.delta.removed_actions.is_empty() {
            reasons.push(format!(
                "removed actions: {}",
                self.delta.removed_actions.join(", ")
            ));
        }
        if reasons.is_empty() {
            reasons.push("unknown structural policy violation".to_string());
        }
        reasons
    }
}

fn extract_mutation_payload(result_text: &str) -> Result<MutationPayload, String> {
    if result_text.trim().is_empty() {
        return Err("TemperAgent completed with empty result".to_string());
    }

    if let Ok(parsed) = serde_json::from_str::<Value>(result_text) {
        if let Some(found) = extract_from_json_value(&parsed) {
            return Ok(found);
        }
    }

    for block in extract_markdown_code_blocks(result_text) {
        if let Ok(parsed) = serde_json::from_str::<Value>(&block)
            && let Some(found) = extract_from_json_value(&parsed)
        {
            return Ok(found);
        }
    }

    Err("TemperAgent result missing MutatedSpecSource JSON payload".to_string())
}

fn extract_from_json_value(v: &Value) -> Option<MutationPayload> {
    let spec = find_first_key(
        v,
        &[
            "MutatedSpecSource",
            "mutated_spec_source",
            "SpecSource",
            "spec_source",
            "new_spec",
        ],
    )?
    .as_str()?
    .to_string();

    let summary = find_first_key(
        v,
        &[
            "MutationSummary",
            "mutation_summary",
            "summary",
            "rationale",
            "change_summary",
        ],
    )
    .and_then(|s| s.as_str().map(str::to_string))
    .unwrap_or_else(|| "Mutation proposed by TemperAgent".to_string());

    let unmet_intent_suggestions = find_first_key(
        v,
        &[
            "UnmetIntentSuggestions",
            "unmet_intent_suggestions",
            "missing_capabilities_handoff",
            "unmet_handoff",
        ],
    )
    .map(parse_string_vec)
    .unwrap_or_default();

    Some(MutationPayload {
        mutated_spec_source: spec,
        mutation_summary: summary,
        unmet_intent_suggestions,
    })
}

fn parse_string_vec(value: Value) -> Vec<String> {
    match value {
        Value::Array(items) => items
            .into_iter()
            .filter_map(|v| match v {
                Value::String(s) => Some(s),
                Value::Number(n) => Some(n.to_string()),
                Value::Bool(b) => Some(b.to_string()),
                _ => None,
            })
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        Value::String(s) => s
            .split(',')
            .map(|p| p.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

fn extract_dataset_missing_capabilities(dataset_json: &str) -> Vec<String> {
    let parsed = serde_json::from_str::<Value>(dataset_json).unwrap_or(Value::Null);
    let missing = parsed
        .get("patterns")
        .and_then(|p| p.get("missing_capabilities"))
        .cloned()
        .unwrap_or(Value::Null);
    let mut out = parse_string_vec(missing);
    out.sort();
    out.dedup();
    out
}

fn validate_optimizer_only_spec_mutation(base_spec: &str, mutated_spec: &str) -> OptimizerOnlyGate {
    let base = parse_spec_shape(base_spec);
    let mutated = parse_spec_shape(mutated_spec);

    let delta = SpecShapeDelta {
        added_states: set_difference(&mutated.states, &base.states),
        removed_states: set_difference(&base.states, &mutated.states),
        added_actions: set_difference(&mutated.actions, &base.actions),
        removed_actions: set_difference(&base.actions, &mutated.actions),
        from_automaton_name: base.automaton_name.clone(),
        to_automaton_name: mutated.automaton_name.clone(),
    };

    let allowed = delta.from_automaton_name == delta.to_automaton_name
        && delta.added_states.is_empty()
        && delta.removed_states.is_empty()
        && delta.added_actions.is_empty()
        && delta.removed_actions.is_empty();

    OptimizerOnlyGate { allowed, delta }
}

fn parse_spec_shape(spec_source: &str) -> SpecShape {
    let lines: Vec<&str> = spec_source.lines().collect();
    let mut automaton_name = None;
    let mut states = std::collections::BTreeSet::new();
    let mut actions = std::collections::BTreeSet::new();

    let mut i = 0usize;
    while i < lines.len() {
        let line = lines[i].trim();
        if line == "[automaton]" {
            i += 1;
            while i < lines.len() {
                let cur = lines[i].trim();
                if cur.starts_with('[') {
                    break;
                }
                if automaton_name.is_none() && cur.starts_with("name") {
                    automaton_name = extract_first_quoted(cur);
                }
                if cur.starts_with("states") {
                    let mut buf = cur.to_string();
                    while !buf.contains(']') && i + 1 < lines.len() {
                        i += 1;
                        buf.push_str(lines[i].trim());
                    }
                    for s in extract_quoted_values(&buf) {
                        states.insert(s);
                    }
                }
                i += 1;
            }
            break;
        }
        i += 1;
    }

    let mut j = 0usize;
    while j < lines.len() {
        let line = lines[j].trim();
        if line == "[[action]]" {
            j += 1;
            while j < lines.len() {
                let cur = lines[j].trim();
                if cur.starts_with('[') {
                    break;
                }
                if cur.starts_with("name") {
                    if let Some(name) = extract_first_quoted(cur) {
                        actions.insert(name);
                    }
                    break;
                }
                j += 1;
            }
            continue;
        }
        j += 1;
    }

    SpecShape {
        automaton_name,
        states,
        actions,
    }
}

fn extract_first_quoted(line: &str) -> Option<String> {
    let mut start = None;
    for (idx, ch) in line.char_indices() {
        if ch == '"' {
            if let Some(s) = start {
                if idx > s {
                    return Some(line[s + 1..idx].to_string());
                }
                start = None;
            } else {
                start = Some(idx);
            }
        }
    }
    None
}

fn extract_quoted_values(raw: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut start = None;
    for (idx, ch) in raw.char_indices() {
        if ch == '"' {
            if let Some(s) = start {
                if idx > s + 1 {
                    values.push(raw[s + 1..idx].to_string());
                }
                start = None;
            } else {
                start = Some(idx);
            }
        }
    }
    values
}

fn set_difference(
    left: &std::collections::BTreeSet<String>,
    right: &std::collections::BTreeSet<String>,
) -> Vec<String> {
    left.difference(right).cloned().collect()
}

fn collect_unmet_intent_handoff(
    dataset_missing: &[String],
    payload_suggestions: &[String],
    gate: &OptimizerOnlyGate,
) -> Vec<String> {
    let mut set = std::collections::BTreeSet::new();
    for item in dataset_missing {
        let trimmed = item.trim();
        if !trimmed.is_empty() {
            set.insert(trimmed.to_string());
        }
    }
    for item in payload_suggestions {
        let trimmed = item.trim();
        if !trimmed.is_empty() {
            set.insert(trimmed.to_string());
        }
    }
    for action in &gate.delta.added_actions {
        set.insert(format!("Add action '{action}'"));
    }
    for state in &gate.delta.added_states {
        set.insert(format!("Add state '{state}'"));
    }
    if gate.delta.from_automaton_name != gate.delta.to_automaton_name
        && let Some(name) = gate.delta.to_automaton_name.as_ref()
    {
        set.insert(format!("Add entity '{name}'"));
    }
    set.into_iter().collect()
}

fn report_unmet_intents(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    skill_name: &str,
    entity_type: &str,
    intents: &[String],
    reason: &str,
) -> Value {
    if intents.is_empty() {
        return json!({
            "attempted": 0,
            "reported": 0,
            "failed": 0,
            "details": [],
        });
    }

    let url = format!("{base_url}/api/evolution/trajectories/unmet");
    let mut reported = 0usize;
    let mut failed = 0usize;
    let mut details = Vec::new();

    for intent in intents {
        let payload = json!({
            "tenant": ctx.tenant,
            "entity_type": entity_type,
            "action": intent,
            "intent": intent,
            "source": "platform",
            "error": reason,
            "request_body": {
                "skill_name": skill_name,
                "target_entity_type": entity_type,
                "origin": "gepa-proposer-agent",
            },
        });
        match ctx.http_call("POST", &url, headers, &payload.to_string()) {
            Ok(resp) if (200..300).contains(&resp.status) => {
                reported += 1;
                details.push(json!({
                    "intent": intent,
                    "status": "reported",
                }));
            }
            Ok(resp) => {
                failed += 1;
                details.push(json!({
                    "intent": intent,
                    "status": "failed",
                    "http_status": resp.status,
                    "body": resp.body,
                }));
            }
            Err(err) => {
                failed += 1;
                details.push(json!({
                    "intent": intent,
                    "status": "failed",
                    "error": err,
                }));
            }
        }
    }

    json!({
        "attempted": intents.len(),
        "reported": reported,
        "failed": failed,
        "details": details,
    })
}

fn find_first_key(root: &Value, keys: &[&str]) -> Option<Value> {
    for key in keys {
        if let Some(value) = find_key_recursive(root, key) {
            return Some(value);
        }
    }
    None
}

fn find_key_recursive(value: &Value, key: &str) -> Option<Value> {
    match value {
        Value::Object(map) => {
            if let Some(found) = map.get(key) {
                return Some(found.clone());
            }
            for nested in map.values() {
                if let Some(found) = find_key_recursive(nested, key) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(arr) => {
            for nested in arr {
                if let Some(found) = find_key_recursive(nested, key) {
                    return Some(found);
                }
            }
            None
        }
        _ => None,
    }
}

fn extract_markdown_code_blocks(text: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut cursor = 0usize;
    let bytes = text.as_bytes();

    while let Some(start_rel) = text[cursor..].find("```") {
        let fence_start = cursor + start_rel;
        let mut line_end = fence_start + 3;
        while line_end < bytes.len() && bytes[line_end] != b'\n' {
            line_end += 1;
        }
        if line_end >= bytes.len() {
            break;
        }
        let content_start = line_end + 1;
        let Some(end_rel) = text[content_start..].find("```") else {
            break;
        };
        let content_end = content_start + end_rel;
        blocks.push(text[content_start..content_end].trim().to_string());
        cursor = content_end + 3;
    }

    blocks
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE_SPEC: &str = r#"
[automaton]
name = "Issue"
states = ["Open", "Assigned", "Closed"]
initial = "Open"

[[action]]
name = "Assign"
kind = "input"
from = ["Open"]
to = "Assigned"

[[action]]
name = "Close"
kind = "input"
from = ["Assigned"]
to = "Closed"
"#;

    #[test]
    fn optimizer_gate_allows_non_structural_change() {
        let mutated = BASE_SPEC.replace("to = \"Assigned\"", "to = \"Open\"");
        let gate = validate_optimizer_only_spec_mutation(BASE_SPEC, &mutated);
        assert!(gate.allowed);
    }

    #[test]
    fn optimizer_gate_blocks_added_action() {
        let mutated = format!(
            "{BASE_SPEC}\n[[action]]\nname = \"Reassign\"\nkind = \"input\"\nfrom = [\"Assigned\"]\nto = \"Assigned\"\n"
        );
        let gate = validate_optimizer_only_spec_mutation(BASE_SPEC, &mutated);
        assert!(!gate.allowed);
        assert_eq!(gate.delta.added_actions, vec!["Reassign".to_string()]);
    }

    #[test]
    fn optimizer_gate_blocks_added_state() {
        let mutated = BASE_SPEC.replace(
            "states = [\"Open\", \"Assigned\", \"Closed\"]",
            "states = [\"Open\", \"Assigned\", \"Closed\", \"Critical\"]",
        );
        let gate = validate_optimizer_only_spec_mutation(BASE_SPEC, &mutated);
        assert!(!gate.allowed);
        assert_eq!(gate.delta.added_states, vec!["Critical".to_string()]);
    }

    #[test]
    fn dataset_missing_capabilities_extracts_array() {
        let raw = r#"{"patterns":{"missing_capabilities":["Reassign","PromoteToCritical"]}}"#;
        let out = extract_dataset_missing_capabilities(raw);
        assert_eq!(
            out,
            vec!["PromoteToCritical".to_string(), "Reassign".to_string()]
        );
    }
}

fn sleep_tick(
    ctx: &Context,
    sandbox_url: &str,
    workdir: &str,
    sleep_ms: u64,
) -> Result<(), String> {
    let secs = sleep_ms as f64 / 1000.0;
    let cmd = format!("sleep {secs:.3}");
    let url = format!("{sandbox_url}/v1/processes/run");
    let headers = vec![("Content-Type".to_string(), "application/json".to_string())];
    let body = json!({
        "command": cmd,
        "workdir": workdir,
    });

    let resp = ctx.http_call("POST", &url, &headers, &body.to_string())?;
    if !(200..300).contains(&resp.status) {
        return Err(format!(
            "sandbox sleep tick failed: HTTP {} body={}",
            resp.status, resp.body
        ));
    }
    Ok(())
}
