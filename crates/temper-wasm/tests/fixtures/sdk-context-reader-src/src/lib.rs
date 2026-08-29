use temper_wasm_sdk::prelude::*;

#[derive(serde::Deserialize)]
struct MemberState {
    status: String,
    large_blob: String,
    attempts: usize,
    ready: bool,
    tags: Vec<String>,
}

temper_module! {
    fn run(ctx: Context) -> Result<Value> {
        let state = ctx
            .member_state::<MemberState>()
            .map_err(|error| error.to_string())?;
        Ok(json!({
            "tenant": ctx.tenant,
            "entity_id": ctx.entity_id,
            "trigger_action": ctx.trigger_action,
            "status": state.status,
            "large_blob_len": state.large_blob.len(),
            "attempts": state.attempts,
            "ready": state.ready,
            "tags": state.tags,
        }))
    }
}
