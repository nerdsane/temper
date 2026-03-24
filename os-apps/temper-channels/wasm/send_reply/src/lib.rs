use temper_wasm_sdk::prelude::*;

#[unsafe(no_mangle)]
pub extern "C" fn run(_ctx_ptr: i32, _ctx_len: i32) -> i32 {
    let result = (|| -> Result<(), String> {
        let ctx = Context::from_host()?;
        let fields = ctx.entity_state.get("fields").cloned().unwrap_or_else(|| json!({}));
        let webhook_url = str_field(&fields, &["webhook_url", "WebhookUrl"]).unwrap_or("");
        let thread_id = str_field(&fields, &["thread_id", "ThreadId"]).unwrap_or("");
        let content = str_field(&fields, &["content", "Content"]).unwrap_or("");
        let agent_entity_id =
            str_field(&fields, &["agent_entity_id", "AgentEntityId"]).unwrap_or("");

        if webhook_url.is_empty() {
            return Err("send_reply: webhook_url is empty".to_string());
        }

        let body = json!({
            "thread_id": thread_id,
            "content": content,
            "agent_entity_id": agent_entity_id,
        });
        let headers = vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("x-tenant-id".to_string(), ctx.tenant.clone()),
        ];
        let resp = ctx.http_call("POST", webhook_url, &headers, &body.to_string())?;
        if !(200..300).contains(&resp.status) {
            return Err(format!("send_reply: webhook POST failed (HTTP {})", resp.status));
        }

        set_success_result(
            "ReplyDelivered",
            &json!({
                "thread_id": thread_id,
                "content": content,
                "agent_entity_id": agent_entity_id,
            }),
        );
        Ok(())
    })();

    if let Err(error) = result {
        set_error_result(&error);
    }
    0
}

fn str_field<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
}
