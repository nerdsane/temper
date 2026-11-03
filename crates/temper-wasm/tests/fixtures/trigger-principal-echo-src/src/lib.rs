//! Answers with the principal its triggering move was made as, as JSON in
//! the `seen` parameter ("null" when the host gave none), so a test can read
//! it back from the record its callback moves.

use temper_wasm_sdk::prelude::*;

temper_module! {
    fn run(ctx: Context) -> Result<Value> {
        let seen = serde_json::to_string(&ctx.trigger_principal).map_err(|e| e.to_string())?;
        Ok(json!({ "seen": seen }))
    }
}
