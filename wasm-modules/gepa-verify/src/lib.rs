//! GEPA verification bridge module.
//!
//! Runs server-side verification for the mutated spec's entity and routes
//! callback to `RecordVerificationPass` or `RecordVerificationFailure`.

use serde_json::Value;
use temper_wasm_sdk::prelude::*;

#[unsafe(no_mangle)]
pub extern "C" fn run(_ctx_ptr: i32, _ctx_len: i32) -> i32 {
    let result = (|| -> Result<(String, Value), String> {
        let ctx = Context::from_host().map_err(|e| e.to_string())?;
        execute(ctx)
    })();

    match result {
        Ok((action, params)) => set_success_result(&action, &params),
        Err(e) => set_error_result(&e),
    }

    0
}

fn execute(ctx: Context) -> Result<(String, Value), String> {
    let fields = ctx.entity_state.get("fields").unwrap_or(&ctx.entity_state);
    let mutated_spec = fields
        .get("MutatedSpecSource")
        .and_then(Value::as_str)
        .or_else(|| ctx.trigger_params.get("MutatedSpecSource").and_then(Value::as_str))
        .ok_or("missing MutatedSpecSource in EvolutionRun state")?;

    let entity_name =
        parse_automaton_name(mutated_spec).ok_or("unable to parse [automaton].name from MutatedSpecSource")?;

    let base_url = ctx
        .config
        .get("temper_api_url")
        .cloned()
        .unwrap_or_else(|| "http://127.0.0.1:3000".to_string());
    let url = format!("{base_url}/observe/verify/{entity_name}");

    let mut headers = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        ("X-Tenant-Id".to_string(), ctx.tenant.clone()),
        ("x-temper-principal-kind".to_string(), "admin".to_string()),
        ("x-temper-principal-id".to_string(), "gepa-verify".to_string()),
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

    let resp = ctx.http_call("POST", &url, &headers, "")?;
    if resp.status != 200 {
        return Err(format!(
            "verification request failed for entity '{entity_name}': HTTP {} {}",
            resp.status, resp.body
        ));
    }

    let parsed: Value =
        serde_json::from_str(&resp.body).map_err(|e| format!("failed to parse verification response JSON: {e}"))?;
    let all_passed = parsed
        .get("all_passed")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let summary = summarize_verification(&parsed);

    if all_passed {
        Ok((
            "RecordVerificationPass".to_string(),
            json!({
                "VerificationReport": summary,
            }),
        ))
    } else {
        Ok((
            "RecordVerificationFailure".to_string(),
            json!({
                "VerificationErrors": summary,
            }),
        ))
    }
}

fn parse_automaton_name(spec: &str) -> Option<String> {
    let mut in_automaton = false;
    for raw in spec.lines() {
        let line = raw.trim();
        if line == "[automaton]" {
            in_automaton = true;
            continue;
        }
        if in_automaton {
            if line.starts_with('[') {
                return None;
            }
            if line.starts_with("name") {
                return extract_first_quoted(line);
            }
        }
    }
    None
}

fn extract_first_quoted(line: &str) -> Option<String> {
    let mut start = None;
    for (idx, ch) in line.char_indices() {
        if ch == '"' {
            if let Some(s) = start {
                if idx > s + 1 {
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

fn summarize_verification(parsed: &Value) -> String {
    let all_passed = parsed
        .get("all_passed")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let levels = parsed
        .get("levels")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut failed = Vec::new();
    let mut passed = 0usize;
    for level in levels {
        let name = level
            .get("level")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let is_passed = level
            .get("passed")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if is_passed {
            passed += 1;
        } else {
            let summary = level
                .get("summary")
                .and_then(Value::as_str)
                .unwrap_or("failed");
            failed.push(format!("{name}: {summary}"));
        }
    }

    if all_passed {
        format!("verification passed: {passed} levels passed")
    } else if failed.is_empty() {
        "verification failed: no detailed levels returned".to_string()
    } else {
        format!("verification failed: {}", failed.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_automaton_name_reads_name() {
        let src = "[automaton]\nname = \"Issue\"\nstates=[\"A\"]";
        assert_eq!(parse_automaton_name(src).as_deref(), Some("Issue"));
    }

    #[test]
    fn summarize_verification_failure_lists_levels() {
        let v = json!({
            "all_passed": false,
            "levels": [
                {"level": "L0", "passed": true, "summary": "ok"},
                {"level": "L1", "passed": false, "summary": "counterexample"},
            ]
        });
        let s = summarize_verification(&v);
        assert!(s.contains("verification failed"));
        assert!(s.contains("L1"));
    }
}
