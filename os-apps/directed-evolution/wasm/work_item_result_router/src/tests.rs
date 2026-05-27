#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_stdout_json_for_variant_app_ref() {
        let raw = json!({
            "stdout": "{\"outputs\":{\"app_ref\":\"owner/app@abc\",\"changed_files\":[\"app.ts\"]}}"
        })
        .to_string();
        let parsed = parse_work_item_output(&raw);

        assert_eq!(
            lookup_string_deep(&parsed, &["app_ref", "appRef", "AppRef"]),
            "owner/app@abc"
        );
        assert_eq!(
            lookup_value_deep(&parsed, &["changed_files"]).expect("changed_files is present"),
            json!(["app.ts"])
        );
    }

    #[test]
    fn evaluation_prompt_preserves_goalpost_boundary() {
        let prompt = format_evaluation_prompt(
            &json!({
                "StageName": "review",
                "StageKind": "reviewer",
                "RequiredEvidenceJson": "{}",
            }),
            "AdaptationGoal goal-1: improve clarity",
            "https://genesis-production-164d.up.railway.app",
            "var-1",
            "gen-1",
            "ep-1",
            "stage-1",
            "summary",
            "app@1",
            "temper://tenant/de-variant/app/app@1",
        );

        assert!(prompt.contains("VariantId: var-1"));
        assert!(prompt.contains("AdaptationGoal goal-1"));
        assert!(prompt.contains("Do not modify evaluators"));
    }

    #[test]
    fn stage_evaluation_fails_closed_without_positive_result() {
        assert!(!stage_evaluation_passed(&json!({
            "summary": "looks reasonable"
        })));
        assert!(!stage_evaluation_passed(&json!({
            "status": "succeeded",
            "summary": "codex command completed"
        })));
        assert!(stage_evaluation_passed(&json!({ "passed": true })));
        assert!(stage_evaluation_passed(&json!({ "status": "passed" })));
    }

    #[test]
    fn selector_requires_surviving_winner() {
        let survivor_ids = vec!["variant-a".to_string(), "variant-b".to_string()];

        assert_eq!(
            select_requested_winner(&json!({ "winning_variant_id": "variant-b" }), &survivor_ids)
                .expect("winner should be accepted"),
            "variant-b"
        );
        assert!(select_requested_winner(&json!({}), &survivor_ids).is_err());
        assert!(
            select_requested_winner(&json!({ "winning_variant_id": "variant-c" }), &survivor_ids)
                .is_err()
        );
    }

    #[test]
    fn followup_prompt_carries_prior_elimination_evidence() {
        let outcomes = vec![VariantOutcome {
            id: "variant-a".to_string(),
            status: "Eliminated".to_string(),
            app_ref: "owner/app@a".to_string(),
            branch_ref: "variant-a".to_string(),
            summary: "Changed only CSDL navigation.".to_string(),
            evidence_summary:
                "Eliminated: runtime stored question_id but CSDL expected QuestionId".to_string(),
            complete: true,
            survived: false,
        }];
        let context = eliminated_generation_evidence_context(&outcomes);
        let prompt_context = format!("Previous Generation Evidence (gen-1):\n{context}");
        let prompt = followup_variant_generator_prompt(FollowupPromptInput {
            episode_id: "ep-1",
            generation_id: "gen-2",
            previous_generation_id: "gen-1",
            organism_id: "org-agent-answers",
            direction_id: "dir-1",
            parent_version_id: "ov-1",
            variant_index: 1,
            variant_target_count: 3,
            prompt_context: &prompt_context,
        });

        assert!(prompt.contains("PreviousGenerationId: gen-1"));
        assert!(prompt.contains("runtime stored question_id"));
        assert!(prompt.contains("aligning persisted IOA field names"));
        assert!(prompt.contains("Do not change evaluation rules"));
    }

    #[test]
    fn promoter_prompt_names_canonical_materialization_target() {
        let prompt = promoter_prompt(
            "promotion-1",
            "episode-1",
            "variant-1",
            "org-agent-answers",
            "nerdsane/agent-answers@abc123",
            "directed-evolution/work-item-1",
        );

        assert!(prompt.contains("PromotionId: promotion-1"));
        assert!(prompt.contains("WinningVariantId: variant-1"));
        assert!(prompt.contains("AppRef: nerdsane/agent-answers@abc123"));
        assert!(prompt.contains("push the winning commit to the canonical Genesis ref"));
    }

    #[test]
    fn materialization_record_prefers_worker_runtime_ref() {
        let record = promotion_materialization_record(
            &json!({
                "canonical_app_ref": "nerdsane/agent-answers@abc123",
                "production_tenant": "default",
                "runtime_ref": "temper://tenant/default/app/nerdsane/agent-answers@abc123",
                "summary": "Published and installed winner",
                "evidence_refs": ["temper://promotion/proof"]
            }),
            "nerdsane/agent-answers@fallback",
        );

        assert_eq!(record.canonical_app_ref, "nerdsane/agent-answers@abc123");
        assert_eq!(record.production_tenant, "default");
        assert_eq!(
            record.runtime_ref,
            "temper://tenant/default/app/nerdsane/agent-answers@abc123"
        );
        assert_eq!(record.evidence_uri, "temper://promotion/proof");
    }

    #[test]
    fn repair_autostart_requires_repair_auto_lane() {
        assert!(repair_autostart_lane_allowed("repair", "repair-auto"));
        assert!(repair_autostart_lane_allowed(
            "performance_regression",
            "repair-automatic"
        ));
        assert!(!repair_autostart_lane_allowed(
            "growth",
            "growth-human-gated"
        ));
        assert!(!repair_autostart_lane_allowed(
            "repair",
            "growth-auto-feature"
        ));
    }

    #[test]
    fn repair_autostart_policy_honors_active_lane_text() {
        assert!(policy_permits_repair_autostart(
            r#"{"repair_lane":"automatic for failing checks after evidence"}"#
        ));
        assert!(!policy_permits_repair_autostart(
            r#"{"repair_lane":"human approval required"}"#
        ));
        assert!(!policy_permits_repair_autostart(
            r#"{"repair_lane":"blocked"}"#
        ));
    }

    #[test]
    fn repair_evaluation_stages_keep_review_and_simulated_user() {
        let mut stages = evaluation_stages_from_output(&json!({
            "evaluation_stages": [
                { "stage_name": "Static repair review", "stage_kind": "reviewer" }
            ]
        }));

        ensure_required_repair_stages(&mut stages);

        assert!(stages.iter().any(|stage| stage.name == "Static repair review"));
        assert!(
            stages
                .iter()
                .any(|stage| stage.kind.contains("simulated_user"))
        );
    }
}
