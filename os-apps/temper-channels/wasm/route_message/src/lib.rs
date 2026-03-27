use temper_wasm_sdk::prelude::*;

#[unsafe(no_mangle)]
pub extern "C" fn run(_ctx_ptr: i32, _ctx_len: i32) -> i32 {
    let result = (|| -> Result<(), String> {
        let ctx = Context::from_host()?;
        let fields = ctx.entity_state.get("fields").cloned().unwrap_or_else(|| json!({}));
        let temper_api_url = resolve_temper_api_url(&ctx, &fields);
        let channel_id = str_field(&fields, &["channel_id", "ChannelId"]).unwrap_or("");
        let default_agent_config =
            str_field(&fields, &["default_agent_config", "DefaultAgentConfig"]).unwrap_or("{}");
        let thread_id = str_field(&fields, &["thread_id", "ThreadId"]).unwrap_or("");
        let author_id = str_field(&fields, &["author_id", "AuthorId"]).unwrap_or("");
        let content = str_field(&fields, &["content", "Content"]).unwrap_or("");
        if channel_id.is_empty() || thread_id.is_empty() || author_id.is_empty() {
            return Err("route_message: missing channel_id/thread_id/author_id".to_string());
        }

        let existing_session = find_active_session(&ctx, &temper_api_url, &ctx.tenant, channel_id, thread_id, author_id)?;
        let agent_id = if let Some(session) = existing_session {
            let session_id = session
                .get("entity_id")
                .and_then(|v| v.as_str())
                .or_else(|| nested_str_field(&session, &["Id"]))
                .unwrap_or_default()
                .to_string();
            let agent_id = nested_str_field(&session, &["AgentEntityId"])
                .unwrap_or_default()
                .to_string();

            // Try to steer the existing agent. If it fails (agent in terminal
            // state), expire the session and create a fresh agent.
            let steer_ok = if !agent_id.is_empty() {
                resume_session(&ctx, &temper_api_url, &ctx.tenant, &session_id).ok();
                steer_existing_agent(&ctx, &temper_api_url, &ctx.tenant, &agent_id, content).is_ok()
            } else {
                false
            };

            if steer_ok {
                agent_id
            } else {
                // Expire the stale session and fall through to create new agent.
                let _ = ctx.http_call(
                    "POST",
                    &format!("{temper_api_url}/tdata/ChannelSessions('{session_id}')/Temper.Claw.ChannelSession.Expire"),
                    &odata_headers(&ctx.tenant),
                    "{}",
                );
                // Fall through to create new agent below
                let route = find_route(&ctx, &temper_api_url, &ctx.tenant, channel_id)?;
                let route_config = route
                    .as_ref()
                    .and_then(|value| nested_str_field(value, &["AgentConfig"]))
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or(default_agent_config);
                let route_soul_id = route
                    .as_ref()
                    .and_then(|value| nested_str_field(value, &["SoulId"]))
                    .unwrap_or("");
                let new_agent_id = create_agent_from_route(
                    &ctx, &temper_api_url, &ctx.tenant,
                    route_config, route_soul_id, content,
                )?;
                create_session(
                    &ctx, &temper_api_url, &ctx.tenant,
                    channel_id, thread_id, author_id, &new_agent_id,
                )?;
                new_agent_id
            }
        } else {
            let route = find_route(&ctx, &temper_api_url, &ctx.tenant, channel_id)?;
            let route_config = route
                .as_ref()
                .and_then(|value| nested_str_field(value, &["AgentConfig"]))
                .filter(|value| !value.trim().is_empty())
                .unwrap_or(default_agent_config);
            let route_soul_id = route
                .as_ref()
                .and_then(|value| nested_str_field(value, &["SoulId"]))
                .unwrap_or("");
            let agent_id = create_agent_from_route(
                &ctx,
                &temper_api_url,
                &ctx.tenant,
                route_config,
                route_soul_id,
                content,
            )?;
            create_session(
                &ctx,
                &temper_api_url,
                &ctx.tenant,
                channel_id,
                thread_id,
                author_id,
                &agent_id,
            )?;
            agent_id
        };

        let result_text = wait_for_agent(&ctx, &temper_api_url, &ctx.tenant, &agent_id)?;
        set_success_result(
            "SendReply",
            &json!({
                "thread_id": thread_id,
                "content": result_text,
                "agent_entity_id": agent_id,
            }),
        );
        Ok(())
    })();

    if let Err(error) = result {
        set_error_result(&error);
    }
    0
}

fn resolve_temper_api_url(ctx: &Context, fields: &Value) -> String {
    fields
        .get("temper_api_url")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| ctx.config.get("temper_api_url").filter(|s| !s.is_empty()).cloned())
        .unwrap_or_else(|| "http://127.0.0.1:3000".to_string())
}

fn odata_headers(tenant: &str) -> Vec<(String, String)> {
    vec![
        ("x-tenant-id".to_string(), tenant.to_string()),
        ("x-temper-principal-kind".to_string(), "admin".to_string()),
        ("content-type".to_string(), "application/json".to_string()),
        ("accept".to_string(), "application/json".to_string()),
    ]
}

fn list_entities(ctx: &Context, url: &str, tenant: &str) -> Result<Vec<Value>, String> {
    let resp = ctx.http_call("GET", url, &odata_headers(tenant), "")?;
    if resp.status != 200 {
        return Err(format!("GET {url} failed (HTTP {})", resp.status));
    }
    let parsed: Value = serde_json::from_str(&resp.body).unwrap_or_else(|_| json!({ "value": [] }));
    Ok(parsed
        .get("value")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

fn find_active_session(
    ctx: &Context,
    temper_api_url: &str,
    tenant: &str,
    channel_id: &str,
    thread_id: &str,
    _author_id: &str,
) -> Result<Option<Value>, String> {
    let filter = format!(
        "$filter=Status eq 'Active' and channel_id eq '{}' and thread_id eq '{}'",
        channel_id, thread_id
    );
    let sessions = list_entities(
        ctx,
        &format!("{temper_api_url}/tdata/ChannelSessions?{filter}"),
        tenant,
    )?;
    Ok(sessions.into_iter().next())
}

fn resume_session(
    ctx: &Context,
    temper_api_url: &str,
    tenant: &str,
    session_id: &str,
) -> Result<(), String> {
    let url = format!(
        "{temper_api_url}/tdata/ChannelSessions('{session_id}')/Temper.Claw.ChannelSession.Resume"
    );
    let _ = ctx.http_call("POST", &url, &odata_headers(tenant), r#"{"last_message_at":"resumed"}"#)?;
    Ok(())
}

fn find_route(
    ctx: &Context,
    temper_api_url: &str,
    tenant: &str,
    channel_id: &str,
) -> Result<Option<Value>, String> {
    let routes = list_entities(ctx, &format!("{temper_api_url}/tdata/AgentRoutes"), tenant)?;
    Ok(routes.into_iter().find(|route| {
        nested_str_field(route, &["Status"]) == Some("Active")
            && {
                let route_channel_id = nested_str_field(route, &["ChannelId"]).unwrap_or("");
                route_channel_id.is_empty() || route_channel_id == channel_id
            }
    }))
}

fn create_agent_from_route(
    ctx: &Context,
    temper_api_url: &str,
    tenant: &str,
    route_config: &str,
    route_soul_id: &str,
    user_message: &str,
) -> Result<String, String> {
    let config: Value = serde_json::from_str(route_config).unwrap_or_else(|_| json!({}));
    let create_resp = ctx.http_call("POST", &format!("{temper_api_url}/tdata/TemperAgents"), &odata_headers(tenant), "{}")?;
    if !(200..300).contains(&create_resp.status) {
        return Err(format!("create TemperAgent failed (HTTP {})", create_resp.status));
    }
    let parsed: Value = serde_json::from_str(&create_resp.body).unwrap_or_else(|_| json!({}));
    let agent_id = parsed
        .get("entity_id")
        .or_else(|| parsed.get("Id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if agent_id.is_empty() {
        return Err("route_message: created TemperAgent missing entity_id".to_string());
    }

    let configure_body = json!({
        "system_prompt": config.get("system_prompt").and_then(Value::as_str).unwrap_or(""),
        "user_message": user_message,
        "model": config.get("model").and_then(Value::as_str).unwrap_or("claude-sonnet-4-20250514"),
        "provider": config.get("provider").and_then(Value::as_str).unwrap_or("anthropic"),
        "tools_enabled": config.get("tools_enabled").and_then(Value::as_str).unwrap_or("read_entity"),
        "max_turns": config.get("max_turns").and_then(Value::as_str).unwrap_or("6"),
        "workdir": config.get("workdir").and_then(Value::as_str).unwrap_or("/tmp/workspace"),
        "soul_id": if route_soul_id.is_empty() {
            config.get("soul_id").and_then(Value::as_str).unwrap_or("")
        } else {
            route_soul_id
        },
    });
    let configure_url = format!(
        "{temper_api_url}/tdata/TemperAgents('{agent_id}')/Temper.Agent.TemperAgent.Configure"
    );
    let configure_resp = ctx.http_call("POST", &configure_url, &odata_headers(tenant), &configure_body.to_string())?;
    if !(200..300).contains(&configure_resp.status) {
        return Err(format!("configure TemperAgent failed (HTTP {})", configure_resp.status));
    }

    let provision_url = format!(
        "{temper_api_url}/tdata/TemperAgents('{agent_id}')/Temper.Agent.TemperAgent.Provision"
    );
    let provision_resp = ctx.http_call("POST", &provision_url, &odata_headers(tenant), "{}")?;
    if !(200..300).contains(&provision_resp.status) {
        return Err(format!("provision TemperAgent failed (HTTP {})", provision_resp.status));
    }
    Ok(agent_id)
}

fn create_session(
    ctx: &Context,
    temper_api_url: &str,
    tenant: &str,
    channel_id: &str,
    thread_id: &str,
    author_id: &str,
    agent_id: &str,
) -> Result<(), String> {
    let create_resp = ctx.http_call(
        "POST",
        &format!("{temper_api_url}/tdata/ChannelSessions"),
        &odata_headers(tenant),
        "{}",
    )?;
    if !(200..300).contains(&create_resp.status) {
        return Err(format!("create ChannelSession failed (HTTP {})", create_resp.status));
    }
    let parsed: Value = serde_json::from_str(&create_resp.body).unwrap_or_else(|_| json!({}));
    let session_id = parsed
        .get("entity_id")
        .or_else(|| parsed.get("Id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if session_id.is_empty() {
        return Err("ChannelSession creation missing entity_id".to_string());
    }
    let create_url = format!(
        "{temper_api_url}/tdata/ChannelSessions('{session_id}')/Temper.Claw.ChannelSession.Create"
    );
    let body = json!({
        "channel_id": channel_id,
        "thread_id": thread_id,
        "author_id": author_id,
        "agent_entity_id": agent_id,
        "last_message_at": "created",
    });
    let resp = ctx.http_call("POST", &create_url, &odata_headers(tenant), &body.to_string())?;
    if !(200..300).contains(&resp.status) {
        return Err(format!("ChannelSession.Create failed (HTTP {})", resp.status));
    }
    Ok(())
}

fn steer_existing_agent(
    ctx: &Context,
    temper_api_url: &str,
    tenant: &str,
    agent_id: &str,
    message: &str,
) -> Result<(), String> {
    let agent_url = format!("{temper_api_url}/tdata/TemperAgents('{agent_id}')");
    let agent_resp = ctx.http_call("GET", &agent_url, &odata_headers(tenant), "")?;
    let mut queue = if agent_resp.status == 200 {
        let parsed: Value = serde_json::from_str(&agent_resp.body).unwrap_or_else(|_| json!({}));
        serde_json::from_str::<Vec<Value>>(
            nested_str_field(&parsed, &["SteeringMessages"]).unwrap_or("[]"),
        )
        .unwrap_or_default()
    } else {
        Vec::new()
    };
    queue.push(json!({ "content": message }));
    let steer_url = format!(
        "{temper_api_url}/tdata/TemperAgents('{agent_id}')/Temper.Agent.TemperAgent.Steer"
    );
    let body = json!({
        "steering_messages": serde_json::to_string(&queue).unwrap_or_else(|_| "[]".to_string()),
    });
    let resp = ctx.http_call("POST", &steer_url, &odata_headers(tenant), &body.to_string())?;
    if !(200..300).contains(&resp.status) {
        return Err(format!("steer agent failed (HTTP {})", resp.status));
    }
    Ok(())
}

fn wait_for_agent(
    ctx: &Context,
    temper_api_url: &str,
    tenant: &str,
    agent_id: &str,
) -> Result<String, String> {
    let wait_url = format!(
        "{temper_api_url}/observe/entities/TemperAgent/{agent_id}/wait?statuses=Completed,Failed,Cancelled&timeout_ms=300000&poll_ms=250"
    );
    let headers = vec![
        ("x-tenant-id".to_string(), tenant.to_string()),
        ("x-temper-principal-kind".to_string(), "admin".to_string()),
        ("accept".to_string(), "application/json".to_string()),
    ];
    let resp = ctx.http_call("GET", &wait_url, &headers, "")?;
    if resp.status != 200 {
        return Err(format!("wait_for_agent failed (HTTP {})", resp.status));
    }
    let parsed: Value = serde_json::from_str(&resp.body).unwrap_or_else(|_| json!({}));
    Ok(parsed
        .get("fields")
        .and_then(|v| v.get("result"))
        .or_else(|| parsed.get("fields").and_then(|v| v.get("Result")))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string())
}

fn str_field<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
}

fn nested_str_field<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    str_field(value, keys).or_else(|| {
        value.get("fields")
            .and_then(|fields| str_field(fields, keys))
    })
}
