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
            "",
            "https://genesis-production-164d.up.railway.app",
            "var-1",
            "gen-1",
            "ep-1",
            "direction-1",
            "stage-1",
            "stage-result-1",
            "work-item-1",
            "summary",
            "app@1",
            "temper://tenant/de-variant/app/app@1",
        );

        assert!(prompt.contains("VariantId: var-1"));
        assert!(prompt.contains("DirectionId: direction-1"));
        assert!(prompt.contains("x-de-direction-id: direction-1"));
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
    fn simulated_users_are_not_stage_evaluators() {
        assert!(!stage_evaluator_role("simulated_user"));
        assert_eq!(
            evaluator_role_for_stage(&json!({
                "StageKind": "datadog telemetry gate",
                "ExecutorKind": ""
            })),
            "telemetry_evaluator"
        );
        assert_eq!(
            evaluator_role_for_stage(&json!({
                "StageKind": "state verification",
                "ExecutorKind": ""
            })),
            "state_verifier"
        );
    }

    #[test]
    fn datadog_stages_require_structured_datadog_evidence() {
        assert!(stage_requires_datadog(&json!({
            "StageKind": "telemetry",
            "MeasurementProvenance": "datadog-measured",
            "RequiredEvidenceJson": "[]"
        })));
        assert!(!datadog_evidence_satisfies_required_contract(
            &json!({
                "evidence_scope": [{
                    "query": "@directed_evolution.episode_id:ep-1",
                    "time_window": "2026-05-28T00:00:00Z/2026-05-28T00:10:00Z",
                    "result_count": 1,
                    "interpretation": "runtime requests were observed",
                    "zero_result_meaning": "failure"
                }]
            }),
            "brain-judged"
        ));
        assert!(datadog_evidence_satisfies_required_contract(
            &json!({
                "evidence_scope": [{
                    "query": "@directed_evolution.episode_id:ep-1",
                    "time_window": "2026-05-28T00:00:00Z/2026-05-28T00:10:00Z",
                    "result_count": 0,
                    "interpretation": "no runtime errors were found",
                    "zero_result_meaning": "success"
                }]
            }),
            "datadog-measured"
        ));
    }

    #[test]
    fn metric_values_are_parsed_for_threshold_gates() {
        assert_eq!(
            metric_numeric_value(&json!({ "runtime_error_count": "2" }), "runtime_error_count"),
            Some(2.0)
        );
        assert_eq!(
            metric_numeric_value(
                &json!([{ "metric_definition_id": "citation_readability_score", "value": "0.78" }]),
                "citation_readability_score",
            ),
            Some(0.78)
        );
    }

    #[test]
    fn simulated_user_trial_state_overrides_brain_reported_blocker_metric() {
        let counts = trial_state_counts_from_entities(&[
            json!({
                "status": "Succeeded",
                "fields": {
                    "Status": "Succeeded",
                    "Summary": "Reviewer compared the answers."
                }
            }),
            json!({
                "fields": {
                    "Status": "Failed",
                    "Blocker": "Runtime route was unavailable."
                }
            }),
        ]);

        assert_eq!(counts.total, 2);
        assert_eq!(counts.succeeded, 1);
        assert_eq!(counts.failed, 1);
        assert_eq!(counts.blocked, 1);
        assert_eq!(counts.runtime_blocked, 1);
        assert_eq!(counts.app_blocked, 0);

        let mut metrics = json!({
            "simulated_user_blocker_count": {
                "value": 0,
                "provenance_kind": "brain-judged",
                "interpretation": "Evaluator thought blockers were unrelated."
            }
        });
        upsert_metric(
            &mut metrics,
            "simulated_user_blocker_count",
            counts.blocked as f64,
            "trials",
            "state-verified",
            "Recorded simulated-user Trial entities that failed or carried a blocker.",
        );

        assert_eq!(
            metric_numeric_value(&metrics, "simulated_user_blocker_count"),
            Some(1.0)
        );
        assert_eq!(
            metric_provenance_kind(&metrics, "simulated_user_blocker_count").as_deref(),
            Some("state-verified")
        );
    }

    #[test]
    fn simulated_user_trial_state_separates_app_and_runtime_blockers() {
        let counts = trial_state_counts_from_entities(&[
            json!({
                "fields": {
                    "Status": "Failed",
                    "Blocker": "The app did not expose reviewer notes before acceptance.",
                    "BlockerKind": "app-behavior"
                }
            }),
            json!({
                "fields": {
                    "Status": "Failed",
                    "Blocker": "Router-level 404 on /tdata for the runtime tenant.",
                    "BlockerKind": "runtime-access"
                }
            }),
            json!({
                "fields": {
                    "Status": "Failed",
                    "Blocker": "Could not complete the journey.",
                    "BlockerKind": "ambiguous"
                }
            }),
        ]);

        assert_eq!(counts.blocked, 3);
        assert_eq!(counts.app_blocked, 1);
        assert_eq!(counts.runtime_blocked, 1);
        assert_eq!(counts.ambiguous_blocked, 1);
    }
}
