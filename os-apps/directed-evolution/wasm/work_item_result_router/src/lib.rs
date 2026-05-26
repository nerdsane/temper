#![allow(dead_code)]

include!("../../common.rs");

temper_side_effect_module! {
    fn run(ctx: Context) -> Result<Value> {
        if !matches!(ctx.trigger_action.as_str(), "SucceedWorkItem" | "FailWorkItem") {
            return Err(format!(
                "work_item_result_router: unsupported trigger action {}",
                ctx.trigger_action
            ));
        }

        let work_item_id = entity_id(&ctx);
        let fields = fields(&ctx);
        let role = field_str(&fields, &["Role"]);
        let target_entity_type = field_str(&fields, &["TargetEntityType"]);
        let target_entity_id = field_str(&fields, &["TargetEntityId"]);
        let result_json = field_str(&fields, &["ResultJson"]);
        let output = parse_work_item_output(&result_json);
        let base_url = resolve_api_url(&ctx);
        let headers = odata_headers(&ctx);

        if ctx.trigger_action == "FailWorkItem" {
            return route_failed_work_item(
                &ctx,
                &base_url,
                &headers,
                &work_item_id,
                &role,
                &target_entity_type,
                &target_entity_id,
                &fields,
            );
        }

        match (role.as_str(), target_entity_type.as_str()) {
            ("observer", "Signal") => {
                route_observer(&ctx, &base_url, &headers, &target_entity_id, &fields, &output)
            }
            ("variant_generator", "Generation") => route_variant_generator(
                &ctx,
                &base_url,
                &headers,
                &work_item_id,
                &target_entity_id,
                &fields,
                &output,
            ),
            ("simulated_user", "StageResult") | ("reviewer", "StageResult") => route_stage_result(
                &ctx,
                &base_url,
                &headers,
                &work_item_id,
                &role,
                &target_entity_id,
                &fields,
                &output,
            ),
            ("selector", "Generation") => route_selector(
                &ctx,
                &base_url,
                &headers,
                &target_entity_id,
                &fields,
                &output,
            ),
            _ => Ok(json!({
                "ignored": true,
                "role": role,
                "target_entity_type": target_entity_type,
                "target_entity_id": target_entity_id,
            })),
        }
    }
}

include!("observer.rs");
include!("variant_generator.rs");
include!("stage_result.rs");
include!("failure.rs");
include!("selector.rs");
include!("evaluation.rs");
include!("selection.rs");
include!("prompts.rs");
include!("tests.rs");
