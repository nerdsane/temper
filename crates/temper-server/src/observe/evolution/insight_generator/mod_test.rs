use super::*;
use crate::state::{TrajectoryEntry, trajectory::TrajectorySource};

fn entry(entity_type: &str, action: &str, success: bool) -> TrajectoryEntry {
    TrajectoryEntry {
        timestamp: "2026-01-01T00:00:00Z".to_string(),
        tenant: "test".to_string(),
        entity_type: entity_type.to_string(),
        entity_id: "e1".to_string(),
        action: action.to_string(),
        success,
        from_status: None,
        to_status: None,
        error: None,
        agent_id: None,
        session_id: None,
        authz_denied: None,
        denied_resource: None,
        denied_module: None,
        source: None,
        spec_governed: None,
        agent_type: None,
        request_body: None,
        intent: None,
        matched_policy_ids: None,
    }
}

fn failed_entry(entity_type: &str, action: &str, error: &str) -> TrajectoryEntry {
    TrajectoryEntry {
        error: Some(error.to_string()),
        ..entry(entity_type, action, false)
    }
}

fn authz_denied_entry(entity_type: &str, action: &str) -> TrajectoryEntry {
    TrajectoryEntry {
        authz_denied: Some(true),
        ..entry(entity_type, action, false)
    }
}

fn platform_failed_entry(entity_type: &str, action: &str, error: &str) -> TrajectoryEntry {
    TrajectoryEntry {
        source: Some(TrajectorySource::Platform),
        ..failed_entry(entity_type, action, error)
    }
}

fn failed_entry_with_intent(
    entity_type: &str,
    action: &str,
    error: &str,
    intent: &str,
    agent_id: &str,
    session_id: &str,
) -> TrajectoryEntry {
    TrajectoryEntry {
        error: Some(error.to_string()),
        intent: Some(intent.to_string()),
        agent_id: Some(agent_id.to_string()),
        session_id: Some(session_id.to_string()),
        ..entry(entity_type, action, false)
    }
}

fn success_entry_with_intent(
    entity_type: &str,
    action: &str,
    intent: &str,
    agent_id: &str,
    session_id: &str,
) -> TrajectoryEntry {
    TrajectoryEntry {
        intent: Some(intent.to_string()),
        agent_id: Some(agent_id.to_string()),
        session_id: Some(session_id.to_string()),
        ..entry(entity_type, action, true)
    }
}

#[test]
fn empty_input_returns_empty() {
    assert!(generate_insights(&[]).is_empty());
    assert!(gap_analysis::generate_unmet_intents(&[]).is_empty());
    assert!(generate_feature_requests(&[]).is_empty());
}

#[test]
fn below_threshold_signals_skipped() {
    let entries = vec![entry("Ticket", "Create", true)];
    let insights = generate_insights(&entries);
    assert!(
        insights.is_empty(),
        "signals with total < 2 should be skipped"
    );
}

#[test]
fn entity_set_not_found_open_unmet_intent() {
    let entries = vec![
        failed_entry("Invoice", "Create", "EntitySetNotFound: Invoice"),
        failed_entry("Invoice", "Create", "EntitySetNotFound: Invoice"),
    ];
    let insights = generate_insights(&entries);
    assert!(!insights.is_empty());
    assert!(insights[0].signal.intent.contains("not found"));
    assert!(insights[0].recommendation.contains("Consider creating"));
}

#[test]
fn entity_set_not_found_resolved_by_submit_spec() {
    let entries = vec![
        failed_entry("Invoice", "Create", "EntitySetNotFound: Invoice"),
        failed_entry("Invoice", "Create", "EntitySetNotFound: Invoice"),
        entry("Invoice", "SubmitSpec", true),
    ];
    let insights = generate_insights(&entries);
    assert!(!insights.is_empty());
    let resolved = insights
        .iter()
        .find(|insight| insight.signal.intent.contains("Invoice"))
        .unwrap();
    assert!(resolved.signal.intent.contains("resolved"));
    assert!(resolved.recommendation.contains("submitted"));
}

#[test]
fn authz_denial_above_threshold_generates_insight() {
    let mut entries = Vec::new();
    for _ in 0..4 {
        entries.push(authz_denied_entry("Task", "Delete"));
    }
    entries.push(entry("Task", "Delete", true));

    let insights = generate_insights(&entries);
    let denial_insight = insights
        .iter()
        .find(|insight| insight.signal.intent.contains("denied"));
    assert!(
        denial_insight.is_some(),
        "should generate authz denial insight"
    );
    assert!(
        denial_insight
            .unwrap()
            .recommendation
            .contains("Cedar permit")
    );
}

#[test]
fn authz_denial_below_threshold_no_special_insight() {
    let mut entries = Vec::new();
    entries.push(authz_denied_entry("Task", "Delete"));
    for _ in 0..9 {
        entries.push(entry("Task", "Delete", true));
    }

    let insights = generate_insights(&entries);
    let denial_insight = insights
        .iter()
        .find(|insight| insight.signal.intent.contains("denied"));
    assert!(
        denial_insight.is_none(),
        "should not generate authz denial insight below threshold"
    );
}

#[test]
fn insights_sorted_by_priority_descending() {
    let mut entries = Vec::new();
    for _ in 0..20 {
        entries.push(failed_entry("Order", "Process", "guard rejected"));
    }
    for _ in 0..2 {
        entries.push(entry("User", "Login", false));
    }

    let insights = generate_insights(&entries);
    for window in insights.windows(2) {
        assert!(
            window[0].priority_score >= window[1].priority_score,
            "insights should be sorted by priority descending"
        );
    }
}

#[test]
fn feature_requests_empty_for_non_platform_source() {
    let entries = vec![
        failed_entry("Ticket", "Create", "EntitySetNotFound"),
        failed_entry("Ticket", "Create", "EntitySetNotFound"),
        failed_entry("Ticket", "Create", "EntitySetNotFound"),
    ];
    assert!(
        generate_feature_requests(&entries).is_empty(),
        "non-platform source should not generate FRs"
    );
}

#[test]
fn feature_requests_below_threshold_skipped() {
    let entries = vec![
        platform_failed_entry("Task", "Archive", "EntitySetNotFound"),
        platform_failed_entry("Task", "Archive", "EntitySetNotFound"),
    ];
    assert!(generate_feature_requests(&entries).is_empty());
}

#[test]
fn feature_requests_above_threshold_generated() {
    let entries = vec![
        platform_failed_entry("Report", "Generate", "ActionNotFound: Generate"),
        platform_failed_entry("Report", "Generate", "ActionNotFound: Generate"),
        platform_failed_entry("Report", "Generate", "ActionNotFound: Generate"),
    ];
    let feature_requests = generate_feature_requests(&entries);
    assert_eq!(feature_requests.len(), 1);
    assert!(feature_requests[0].description.contains("Generate"));
    assert_eq!(feature_requests[0].frequency, 3);
}

#[test]
fn unmet_intents_open_vs_resolved() {
    let entries = vec![
        failed_entry("Billing", "Charge", "EntitySetNotFound"),
        failed_entry("Billing", "Charge", "EntitySetNotFound"),
        entry("Billing", "SubmitSpec", true),
    ];
    let intents = gap_analysis::generate_unmet_intents(&entries);
    assert!(!intents.is_empty());
    let billing = intents
        .iter()
        .find(|intent| intent.entity_type == "Billing")
        .unwrap();
    assert_eq!(billing.status, "resolved");
}

#[test]
fn intent_evidence_prefers_explicit_intent_and_detects_workaround() {
    let entries = vec![
        failed_entry_with_intent(
            "Invoice",
            "GenerateInvoice",
            "EntitySetNotFound: Invoice",
            "Send an invoice to the customer",
            "agent-1",
            "session-1",
        ),
        success_entry_with_intent(
            "InvoiceDraft",
            "CreateDraft",
            "Send an invoice to the customer",
            "agent-1",
            "session-1",
        ),
    ];

    let evidence = intent_evidence::generate_intent_evidence(&entries);
    assert_eq!(evidence.intent_candidates.len(), 1);
    assert_eq!(evidence.workaround_patterns.len(), 1);
    assert_eq!(
        evidence.intent_candidates[0].intent_title,
        "Send An Invoice To The Customer"
    );
    assert_eq!(evidence.intent_candidates[0].suggested_kind, "workaround");
    assert_eq!(evidence.intent_candidates[0].workaround_count, 1);
    assert_eq!(evidence.workaround_patterns[0].occurrences, 1);
}

#[test]
fn intent_evidence_marks_abandonment_for_unrecovered_failures() {
    let entries = vec![
        failed_entry_with_intent(
            "Issue",
            "MoveToTodo",
            "Authorization denied",
            "Move issue into active work",
            "worker-1",
            "session-2",
        ),
        failed_entry_with_intent(
            "Issue",
            "MoveToTodo",
            "Authorization denied",
            "Move issue into active work",
            "worker-1",
            "session-2",
        ),
    ];

    let evidence = intent_evidence::generate_intent_evidence(&entries);
    assert_eq!(evidence.intent_candidates.len(), 1);
    assert_eq!(evidence.abandonment_patterns.len(), 1);
    assert_eq!(evidence.intent_candidates[0].abandonment_count, 1);
    assert_eq!(
        evidence.intent_candidates[0].suggested_kind,
        "governance_gap"
    );
}

#[test]
fn categorize_error_patterns() {
    assert_eq!(
        categorize_error(Some("EntitySetNotFound: X")),
        "EntitySetNotFound"
    );
    assert_eq!(
        categorize_error(Some("Authorization denied")),
        "AuthzDenied"
    );
    assert_eq!(
        categorize_error(Some("ActionNotFound: Y")),
        "ActionNotFound"
    );
    assert_eq!(categorize_error(Some("guard rejected")), "GuardRejected");
    assert_eq!(categorize_error(Some("something else")), "Other");
    assert_eq!(categorize_error(None), "Unknown");
}
