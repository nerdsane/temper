use temper_wasm_sdk::prelude::*;

const DIRECTED_EVOLUTION_NAMESPACE: &str = "Temper.DirectedEvolution";

macro_rules! temper_side_effect_module {
    (fn $name:ident($ctx:ident : Context) -> Result<Value> $body:block) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn run(_ctx_ptr: i32, _ctx_len: i32) -> i32 {
            let result = (|| -> std::result::Result<temper_wasm_sdk::Value, String> {
                let $ctx = temper_wasm_sdk::Context::from_host().map_err(|e| e.to_string())?;
                $body
            })();

            match result {
                Ok(val) => {
                    temper_wasm_sdk::set_success_result("", &val);
                }
                Err(error) => {
                    temper_wasm_sdk::set_error_result(&error);
                }
            }
            0
        }
    };
}

fn fields(ctx: &Context) -> Value {
    ctx.entity_state
        .get("fields")
        .cloned()
        .unwrap_or_else(|| json!({}))
}

fn entity_id(ctx: &Context) -> String {
    ctx.entity_state
        .get("entity_id")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string()
}

fn field_str(fields: &Value, keys: &[&str]) -> String {
    for key in keys {
        if let Some(value) = fields.get(*key).and_then(Value::as_str) {
            return value.to_string();
        }
        let lower = lower_first(key);
        if let Some(value) = fields.get(&lower).and_then(Value::as_str) {
            return value.to_string();
        }
    }
    String::new()
}

fn field_u64(fields: &Value, keys: &[&str]) -> u64 {
    for key in keys {
        if let Some(value) = fields.get(*key).and_then(Value::as_u64) {
            return value;
        }
        if let Some(value) = fields
            .get(*key)
            .and_then(Value::as_str)
            .and_then(|raw| raw.parse::<u64>().ok())
        {
            return value;
        }
        let lower = lower_first(key);
        if let Some(value) = fields.get(&lower).and_then(Value::as_u64) {
            return value;
        }
        if let Some(value) = fields
            .get(&lower)
            .and_then(Value::as_str)
            .and_then(|raw| raw.parse::<u64>().ok())
        {
            return value;
        }
    }
    0
}

fn lower_first(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn config_usize(ctx: &Context, key: &str, fallback: usize) -> usize {
    ctx.config
        .get(key)
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(fallback)
}

fn resolve_api_url(ctx: &Context) -> String {
    ctx.config
        .get("temper_api_url")
        .filter(|value| !value.trim().is_empty() && !value.contains("{secret:"))
        .cloned()
        .unwrap_or_else(|| "http://127.0.0.1:3000".to_string())
}

fn resolve_public_api_url(ctx: &Context) -> String {
    ctx.config
        .get("temper_public_api_url")
        .filter(|value| !value.trim().is_empty() && !value.contains("{secret:"))
        .cloned()
        .unwrap_or_else(|| resolve_api_url(ctx))
}

fn odata_headers(ctx: &Context) -> Vec<(String, String)> {
    vec![
        ("content-type".to_string(), "application/json".to_string()),
        ("accept".to_string(), "application/json".to_string()),
        ("x-tenant-id".to_string(), ctx.tenant.clone()),
        ("x-temper-principal-kind".to_string(), "agent".to_string()),
        ("x-temper-principal-id".to_string(), entity_id(ctx)),
        ("x-temper-agent-type".to_string(), "system".to_string()),
    ]
}

fn create_entity(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    entity_set: &str,
) -> Result<String, String> {
    let created = post_json(
        ctx,
        &format!("{base_url}/tdata/{entity_set}"),
        headers,
        json!({}),
    )?;
    entity_id_from_response(&created)
        .ok_or_else(|| format!("create {entity_set}: missing entity_id"))
}

fn get_entity(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    entity_set: &str,
    id: &str,
) -> Result<Value, String> {
    get_json(
        ctx,
        &format!("{base_url}/tdata/{entity_set}('{}')", escape_odata_id(id)),
        headers,
    )
}

fn list_entities(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    entity_set: &str,
    filter: &str,
) -> Result<Vec<Value>, String> {
    let value = get_json(
        ctx,
        &format!("{base_url}/tdata/{entity_set}?$filter={filter}"),
        headers,
    )?;
    Ok(value
        .get("value")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

fn post_directed_action(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    entity_set: &str,
    entity_id: &str,
    action: &str,
    body: Value,
) -> Result<Value, String> {
    let url = format!(
        "{base_url}/tdata/{entity_set}('{}')/{}.{}",
        escape_odata_id(entity_id),
        DIRECTED_EVOLUTION_NAMESPACE,
        action
    );
    post_json(ctx, &url, headers, body)
}

fn post_json(
    ctx: &Context,
    url: &str,
    headers: &[(String, String)],
    body: Value,
) -> Result<Value, String> {
    let resp = ctx.http_call("POST", url, headers, &body.to_string())?;
    parse_json_response(resp, &format!("POST {url}"))
}

fn get_json(ctx: &Context, url: &str, headers: &[(String, String)]) -> Result<Value, String> {
    let resp = ctx.http_call("GET", url, headers, "")?;
    parse_json_response(resp, &format!("GET {url}"))
}

fn parse_json_response(resp: HttpResponse, label: &str) -> Result<Value, String> {
    if resp.status < 200 || resp.status >= 300 {
        return Err(format!(
            "{label} failed: HTTP {} body={}",
            resp.status, resp.body
        ));
    }
    if resp.body.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str::<Value>(&resp.body)
        .map_err(|error| format!("{label}: parse JSON response: {error}"))
}

fn entity_id_from_response(value: &Value) -> Option<String> {
    value
        .get("entity_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            value
                .get("fields")
                .and_then(|fields| fields.get("Id").or_else(|| fields.get("id")))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
}

fn entity_id_from_entity(value: &Value) -> String {
    entity_id_from_response(value).unwrap_or_default()
}

fn entity_status(entity: &Value) -> String {
    entity
        .get("status")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            entity
                .get("Status")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default()
}

fn state_fields(entity: &Value) -> Value {
    entity.get("fields").cloned().unwrap_or_else(|| json!({}))
}

fn parse_json_string_array(value: &str) -> Vec<String> {
    if value.trim().is_empty() {
        return Vec::new();
    }
    serde_json::from_str::<Value>(value)
        .ok()
        .and_then(|parsed| {
            parsed.as_array().map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
        })
        .unwrap_or_else(|| {
            value
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(str::to_string)
                .collect()
        })
}

fn parse_work_item_output(raw: &str) -> Value {
    let parsed = serde_json::from_str::<Value>(raw).unwrap_or_else(|_| json!({ "raw": raw }));
    if let Some(stdout) = parsed.get("stdout").and_then(Value::as_str) {
        if let Some(value) = parse_jsonish(stdout) {
            return value;
        }
    }
    parsed
}

fn parse_jsonish(raw: &str) -> Option<Value> {
    serde_json::from_str::<Value>(raw).ok().or_else(|| {
        let start = raw.find('{')?;
        let end = raw.rfind('}')?;
        if end <= start {
            return None;
        }
        serde_json::from_str::<Value>(&raw[start..=end]).ok()
    })
}

fn lookup_string_deep(value: &Value, keys: &[&str]) -> String {
    lookup_value_deep(value, keys)
        .and_then(|value| {
            value
                .as_str()
                .map(str::to_string)
                .or_else(|| Some(value.to_string()))
        })
        .unwrap_or_default()
}

fn lookup_bool_deep(value: &Value, keys: &[&str]) -> Option<bool> {
    lookup_value_deep(value, keys).and_then(|value| {
        value.as_bool().or_else(|| {
            value.as_str().map(|raw| {
                raw.eq_ignore_ascii_case("true")
                    || raw.eq_ignore_ascii_case("passed")
                    || raw.eq_ignore_ascii_case("success")
            })
        })
    })
}

fn lookup_value_deep(value: &Value, keys: &[&str]) -> Option<Value> {
    if let Some(object) = value.as_object() {
        for key in keys {
            if let Some(found) = object.get(*key) {
                return Some(found.clone());
            }
        }
        for child in object.values() {
            if let Some(found) = lookup_value_deep(child, keys) {
                return Some(found);
            }
        }
    }
    if let Some(array) = value.as_array() {
        for child in array {
            if let Some(found) = lookup_value_deep(child, keys) {
                return Some(found);
            }
        }
    }
    None
}

fn escape_odata_id(id: &str) -> String {
    id.replace('\'', "''")
}

fn short_id(id: &str) -> String {
    id.chars().take(10).collect()
}
