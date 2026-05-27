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
}
