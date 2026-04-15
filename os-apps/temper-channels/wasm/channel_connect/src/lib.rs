use temper_wasm_sdk::prelude::*;

#[unsafe(no_mangle)]
pub extern "C" fn run(_ctx_ptr: i32, _ctx_len: i32) -> i32 {
    let result = (|| -> Result<(), String> {
        let ctx = Context::from_host()?;
        let fields = ctx.entity_state.get("fields").cloned().unwrap_or_else(|| json!({}));
        let channel_type = fields
            .get("channel_type")
            .and_then(|v| v.as_str())
            .unwrap_or("webhook");
        let channel_id = fields
            .get("channel_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        ctx.log(
            "info",
            &format!("channel_connect: ready channel_type={channel_type} channel_id={channel_id}"),
        );
        set_success_result("Ready", &json!({}));
        Ok(())
    })();

    if let Err(error) = result {
        set_error_result(&error);
    }
    0
}
