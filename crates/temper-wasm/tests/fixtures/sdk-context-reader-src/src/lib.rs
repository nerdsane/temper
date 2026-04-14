use temper_wasm_sdk::prelude::*;

temper_module! {
    fn run(ctx: Context) -> Result<Value> {
        Ok(json!({
            "tenant": ctx.tenant,
            "entity_id": ctx.entity_id,
            "trigger_action": ctx.trigger_action,
            "entity_state_len": ctx.entity_state.to_string().len(),
        }))
    }
}
