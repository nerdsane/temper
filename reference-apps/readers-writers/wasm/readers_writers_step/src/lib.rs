//! Readers/Writers WASM transition module.
//!
//! Public protocol actions produce proposals. The ValidateProposal action
//! invokes this same module to validate the proposal and return a narrow
//! IOA callback that Temper can guard with counters and states.

use readers_writers_reference::{
    apply_protocol_action, callback_params, parse_action, proposal_params, rejection_params,
    state_from_fields, validate_proposal,
};
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
    let fields = ctx.entity_state.get("fields").cloned().unwrap_or(json!({}));

    if ctx.trigger_action == "ValidateProposal" {
        return match validate_proposal(&fields, &ctx.trigger_params) {
            Ok(outcome) => {
                let action = outcome.kind.callback_action().to_string();
                let protocol_action = parse_action(
                    ctx.trigger_params
                        .get("last_step")
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                    &ctx.trigger_params,
                )?;
                Ok((action, callback_params(&outcome, &protocol_action)))
            }
            Err(error) => Ok((
                "Rejected".to_string(),
                rejection_params(
                    error,
                    ctx.trigger_params
                        .get("last_step")
                        .and_then(Value::as_str)
                        .unwrap_or("ValidateProposal"),
                    ctx.trigger_params.get("actor").and_then(Value::as_i64),
                ),
            )),
        };
    }

    let state = state_from_fields(&fields);
    let action = parse_action(&ctx.trigger_action, &ctx.trigger_params)?;
    match apply_protocol_action(&state, &action) {
        Ok(outcome) => Ok(("ValidateProposal".to_string(), proposal_params(&outcome, &action))),
        Err(error) => Ok((
            "Rejected".to_string(),
            rejection_params(error, action.name(), action.actor()),
        )),
    }
}
