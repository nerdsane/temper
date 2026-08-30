#![allow(dead_code)]

include!("../../common.rs");

temper_side_effect_module! {
    fn run(ctx: Context) -> Result<Value> {
        if ctx.trigger_action != "StartEpisode" {
            return Err(format!(
                "episode_orchestrator: unsupported trigger action {}",
                ctx.trigger_action
            ));
        }

        let episode_id = entity_id(&ctx);
        let fields = fields(&ctx);
        let base_url = resolve_api_url(&ctx);
        let headers = odata_headers(&ctx);
        let generation_id = create_entity(&ctx, &base_url, &headers, "Generations")?;
        let organism_id = field_str(&fields, &["OrganismId"]);
        let direction_id = field_str(&fields, &["DirectionId"]);
        let parent_version_id = field_str(&fields, &["ParentVersionId"]);
        let variant_target_count = config_usize(&ctx, "variant_target_count", 3);
        let generation_index = field_u64(&fields, &["generation_count", "GenerationCount"]) + 1;
        let prompt_context = variant_generation_context(
            &ctx,
            &base_url,
            &headers,
            &fields,
            &organism_id,
            &direction_id,
            &parent_version_id,
        )?;

        post_directed_action(
            &ctx,
            &base_url,
            &headers,
            "Generations",
            &generation_id,
            "StartGeneration",
            json!({
                "EpisodeId": episode_id,
                "ParentVersionId": parent_version_id,
                "GenerationIndex": generation_index.to_string(),
                "VariantTargetCount": variant_target_count.to_string(),
            }),
        )?;
        post_directed_action(
            &ctx,
            &base_url,
            &headers,
            "Episodes",
            &episode_id,
            "AddGeneration",
            json!({ "GenerationId": generation_id }),
        )?;

        let mut work_item_ids = Vec::new();
        for variant_index in 1..=variant_target_count {
            let work_item_id = create_entity(&ctx, &base_url, &headers, "WorkItems")?;
            let prompt = variant_generator_prompt(
                &episode_id,
                &generation_id,
                &organism_id,
                &direction_id,
                &parent_version_id,
                variant_index,
                variant_target_count,
                &prompt_context,
            );
            post_directed_action(
                &ctx,
                &base_url,
                &headers,
                "WorkItems",
                &work_item_id,
                "QueueWorkItem",
                json!({
                    "Role": "variant_generator",
                    "TargetEntityType": "Generation",
                    "TargetEntityId": generation_id,
                    "PromptRef": format!("literal:{prompt}"),
                    "ContextRef": format!("episode:{episode_id}"),
                    "OutputSchemaRef": "directed-evolution.variant-generator.v1",
                    "CorrelationJson": json!({
                        "episode_id": episode_id,
                        "generation_id": generation_id,
                        "organism_id": organism_id,
                        "direction_id": direction_id,
                        "variant_index": variant_index,
                        "variant_target_count": variant_target_count,
                    }).to_string(),
                }),
            )?;
            work_item_ids.push(work_item_id);
        }

        Ok(json!({
            "generation_id": generation_id,
            "variant_target_count": variant_target_count,
            "work_item_ids_json": work_item_ids,
        }))
    }
}

fn variant_generator_prompt(
    episode_id: &str,
    generation_id: &str,
    organism_id: &str,
    direction_id: &str,
    parent_version_id: &str,
    variant_index: usize,
    variant_target_count: usize,
    prompt_context: &str,
) -> String {
    format!(
        "Generate Directed Evolution variant {variant_index} of {variant_target_count}.\n\
EpisodeId: {episode_id}\n\
GenerationId: {generation_id}\n\
OrganismId: {organism_id}\n\
DirectionId: {direction_id}\n\
ParentVersionId: {parent_version_id}\n\n\
{prompt_context}\n\n\
Variant lane suggestion: {}\n\
Work in the assigned organism repository and create one real candidate variant. \
Keep the mutation bounded to the Agent Answers app bundle: prefer changing APP.md, \
adrs/, specs/question.ioa.toml, specs/answer.ioa.toml, specs/model.csdl.xml, and \
policies/agent_answers.cedar. Do not create unrelated entity families unless the \
lane explicitly requires it. Preserve existing Question and Answer actions. \
For repair episodes, repair the observed failure directly and do not add product-growth \
features, intent-capture affordances, or optional metadata unless they are required by \
the repair and automatically maintained by the existing lifecycle. \
Return JSON with: summary, app_ref, branch_ref, runtime_ref, changed_files, diff_ref, \
verification_notes, and next_actions. Do not change evaluation rules or viability constraints.",
        variant_lane_suggestion(variant_index, prompt_context),
    )
}

fn variant_lane_suggestion(variant_index: usize, prompt_context: &str) -> &'static str {
    if prompt_context_is_repair(prompt_context) {
        return match variant_index {
            1 => {
                "Repair the exact failing lifecycle path with the smallest state-machine change that makes submitted answers discoverable for acceptance. Prefer wiring existing RecordAnswer/Accept semantics over adding optional product-growth fields."
            }
            2 => {
                "Repair answer visibility by adding or adjusting a bounded index or relationship only if it is maintained automatically by existing transitions. Do not require a human or simulated user to call a new action before acceptance can see submitted answers."
            }
            3 => {
                "Repair by adding executable regression coverage and minimal spec/CSDL/Cedar updates that prove Configure -> Submit -> RecordAnswer -> Accept remains visible and actionable. Avoid new intent, evidence, uncertainty, or decision-frame product fields."
            }
            _ => {
                "Make the smallest backward-compatible repair that resolves the observed failure while preserving the existing Question and Answer lifecycle."
            }
        };
    }
    match variant_index {
        1 => {
            "Improve Answer usefulness by adding a compact answer-quality or decision-frame field/action while preserving Submit and Accept."
        }
        2 => {
            "Improve Question intent capture by adding a lightweight intent/context field/action while preserving Configure, RecordAnswer, and Accept."
        }
        3 => {
            "Improve evidence and uncertainty handling by adding a bounded evidence/uncertainty field/action on Answer while preserving legacy behavior."
        }
        _ => {
            "Make a small backward-compatible Question or Answer improvement that helps simulated users evaluate Q&A quality."
        }
    }
}

fn prompt_context_is_repair(prompt_context: &str) -> bool {
    let normalized = prompt_context.to_ascii_lowercase();
    normalized.contains("direction pressure class: repair")
        || normalized.contains("repair-boundary")
        || normalized.contains("bounded repair")
}

fn variant_generation_context(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    episode_fields: &Value,
    organism_id: &str,
    direction_id: &str,
    parent_version_id: &str,
) -> Result<String, String> {
    let mut sections = Vec::new();
    let parent = entity_fields_or_empty(
        ctx,
        base_url,
        headers,
        "OrganismVersions",
        parent_version_id,
    );
    sections.push(format!(
        "Parent AppRef: {}\nParent Summary: {}",
        field_str(&parent, &["AppRef"]),
        field_str(&parent, &["Summary"])
    ));

    let direction = entity_fields_or_empty(ctx, base_url, headers, "Directions", direction_id);
    sections.push(format!(
        "Direction: {}\nDirection Summary: {}\nDirection Pressure Class: {}\nProposed Adaptation Goal: {}",
        field_str(&direction, &["Title"]),
        field_str(&direction, &["Summary"]),
        field_str(&direction, &["PressureClass"]),
        field_str(&direction, &["ProposedAdaptationGoal"])
    ));

    let adaptation_goal_id = field_str(episode_fields, &["AdaptationGoalId"]);
    let adaptation_goal = entity_fields_or_empty(
        ctx,
        base_url,
        headers,
        "AdaptationGoals",
        &adaptation_goal_id,
    );
    if !adaptation_goal_id.is_empty() {
        sections.push(format!(
            "Adaptation Goal: {}",
            field_str(&adaptation_goal, &["GoalStatement"])
        ));
    }

    let selection_pressure_id = field_str(episode_fields, &["SelectionPressureId"]);
    let selection_pressure = entity_fields_or_empty(
        ctx,
        base_url,
        headers,
        "SelectionPressures",
        &selection_pressure_id,
    );
    if !selection_pressure_id.is_empty() {
        sections.push(format!(
            "Selection Pressure: {}",
            field_str(&selection_pressure, &["SelectionStatement"])
        ));
    }

    let constraints =
        parse_json_string_array(&field_str(episode_fields, &["ViabilityConstraintIdsJson"]))
            .into_iter()
            .filter_map(|constraint_id| {
                let fields = entity_fields_or_empty(
                    ctx,
                    base_url,
                    headers,
                    "ViabilityConstraints",
                    &constraint_id,
                );
                let statement = field_str(&fields, &["ConstraintStatement"]);
                (!statement.is_empty()).then_some(format!(
                    "- {} ({})",
                    statement,
                    field_str(&fields, &["ConstraintKind"])
                ))
            })
            .collect::<Vec<_>>();
    if !constraints.is_empty() {
        sections.push(format!(
            "Viability Constraints:\n{}",
            constraints.join("\n")
        ));
    }

    let stages = parse_json_string_array(&field_str(episode_fields, &["EvaluationStageIdsJson"]))
        .into_iter()
        .filter_map(|stage_id| {
            let fields =
                entity_fields_or_empty(ctx, base_url, headers, "EvaluationStages", &stage_id);
            let name = field_str(&fields, &["StageName"]);
            (!name.is_empty()).then_some(format!(
                "- {} [{}]",
                name,
                field_str(&fields, &["StageKind"])
            ))
        })
        .collect::<Vec<_>>();
    if !stages.is_empty() {
        sections.push(format!("Evaluation Stages:\n{}", stages.join("\n")));
    }

    sections.push(format!(
        "OrganismId: {organism_id}\nDo not modify the evaluator, viability constraints, or selection pressure."
    ));
    Ok(sections.join("\n\n"))
}

fn entity_fields_or_empty(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    entity_set: &str,
    entity_id: &str,
) -> Value {
    if entity_id.trim().is_empty() {
        return json!({});
    }
    get_entity(ctx, base_url, headers, entity_set, entity_id)
        .map(|entity| state_fields(&entity))
        .unwrap_or_else(|_| json!({}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variant_prompt_names_episode_and_generation() {
        let prompt = variant_generator_prompt(
            "ep-1",
            "gen-1",
            "org-1",
            "dir-1",
            "ov-1",
            2,
            3,
            "Adaptation Goal: Improve trust.",
        );

        assert!(prompt.contains("EpisodeId: ep-1"));
        assert!(prompt.contains("GenerationId: gen-1"));
        assert!(prompt.contains("variant 2 of 3"));
        assert!(prompt.contains("Improve trust"));
        assert!(prompt.contains("Improve Question intent capture"));
        assert!(prompt.contains("Preserve existing Question and Answer actions"));
        assert!(prompt.contains("Do not change evaluation rules"));
    }

    #[test]
    fn repair_variant_prompt_uses_repair_lanes() {
        let prompt = variant_generator_prompt(
            "ep-1",
            "gen-1",
            "org-1",
            "dir-1",
            "ov-1",
            2,
            3,
            "Direction Pressure Class: repair\nViability Constraints:\n- Preserve lifecycle. (repair-boundary)",
        );

        assert!(prompt.contains("Repair answer visibility"));
        assert!(prompt.contains("Do not require a human or simulated user to call a new action"));
        assert!(prompt.contains("do not add product-growth"));
        assert!(!prompt.contains("Improve Question intent capture"));
    }
}
