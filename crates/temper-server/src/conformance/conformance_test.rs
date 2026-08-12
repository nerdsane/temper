//! Conformance checker tests.
//!
//! Every violation kind gets a case asserting the exact index and kind, so a
//! change in walk order or in what counts as a violation shows up as a failing
//! assertion rather than a silently different report.

use super::*;
use temper_ots::models::{
    DecisionType, OTSChoice, OTSConsequence, OTSDecision, OTSMetadata, OTSTrajectory, OTSTurn,
    OutcomeType,
};
use temper_spec::automaton::parse_automaton;

const ORDER_IOA: &str = include_str!("../../../../test-fixtures/specs/order.ioa.toml");

/// An actor whose only action constrains its source states through a
/// `state_in` guard rather than a `from` list.
const GUARD_ONLY_IOA: &str = r#"
[automaton]
name = "Ticket"
states = ["Open", "Working", "Closed"]
initial = "Open"

[[action]]
name = "Work"
kind = "input"
guard = [{ type = "state_in", values = ["Open"] }]
to = "Working"

[[action]]
name = "Close"
kind = "input"
from = ["Working"]
to = "Closed"
"#;

fn order_automaton() -> temper_spec::automaton::Automaton {
    parse_automaton(ORDER_IOA).expect("order fixture parses")
}

/// A successful entity-sourced row for `action`, moving `from` -> `to`.
fn row(action: &str, from: Option<&str>, to: Option<&str>) -> TursoTrajectoryRow {
    TursoTrajectoryRow {
        tenant: "default".to_string(),
        entity_type: "Order".to_string(),
        entity_id: "order-1".to_string(),
        action: action.to_string(),
        success: true,
        from_status: from.map(str::to_string),
        to_status: to.map(str::to_string),
        error: None,
        agent_id: Some("agent-1".to_string()),
        session_id: Some("session-1".to_string()),
        authz_denied: None,
        denied_resource: None,
        denied_module: None,
        source: Some("Entity".to_string()),
        spec_governed: Some(true),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        request_body: None,
        intent: None,
        matched_policy_ids: None,
    }
}

fn denied(action: &str, from: Option<&str>) -> TursoTrajectoryRow {
    TursoTrajectoryRow {
        success: false,
        to_status: None,
        authz_denied: Some(true),
        error: Some("Cedar denied".to_string()),
        source: Some("Authz".to_string()),
        ..row(action, from, None)
    }
}

fn failed(action: &str, from: Option<&str>) -> TursoTrajectoryRow {
    TursoTrajectoryRow {
        success: false,
        to_status: None,
        error: Some("dispatch failed".to_string()),
        ..row(action, from, None)
    }
}

fn only_violation(report: &ConformanceReport) -> &Violation {
    assert_eq!(
        report.violations.len(),
        1,
        "expected exactly one violation, got {:?}",
        report.violations
    );
    &report.violations[0]
}

#[test]
fn a_legal_run_passes_with_no_violations() {
    let rows = vec![
        row("AddItem", Some("Draft"), Some("Draft")),
        row("SubmitOrder", Some("Draft"), Some("Submitted")),
        row("ConfirmOrder", Some("Submitted"), Some("Confirmed")),
        row("ProcessOrder", Some("Confirmed"), Some("Processing")),
        row("ShipOrder", Some("Processing"), Some("Shipped")),
        row("DeliverOrder", Some("Shipped"), Some("Delivered")),
    ];

    let report = check_conformance(&order_automaton(), &rows, None);

    assert!(report.passed, "violations: {:?}", report.violations);
    assert!(report.violations.is_empty());
    assert_eq!(report.stats.stream_length, 6);
    assert_eq!(report.stats.actor_rows, 6);
    assert_eq!(report.stats.transitions_unchecked, 0);
    assert_eq!(
        report.stats.terminal_entities, 0,
        "Delivered still has InitiateReturn leaving it, so it is not terminal"
    );
    assert!(report.stats.violations_by_kind.is_empty());
}

#[test]
fn illegal_transition_reports_the_offending_index() {
    let rows = vec![
        row("AddItem", Some("Draft"), Some("Draft")),
        // ShipOrder is legal only from Processing.
        row("ShipOrder", Some("Draft"), Some("Shipped")),
    ];

    let report = check_conformance(&order_automaton(), &rows, None);

    assert!(!report.passed);
    let violation = only_violation(&report);
    assert_eq!(violation.index, 1);
    assert_eq!(violation.kind, ViolationKind::IllegalTransition);
    assert_eq!(violation.action, "ShipOrder");
    assert_eq!(violation.entity_type, "Order");
    assert!(
        violation.detail.contains("Draft") && violation.detail.contains("Processing"),
        "detail must name both the observed and the legal states: {}",
        violation.detail
    );
    assert_eq!(report.stats.violations_by_kind["illegal_transition"], 1);
}

#[test]
fn a_state_in_guard_stands_in_for_a_missing_from_list() {
    let automaton = parse_automaton(GUARD_ONLY_IOA).expect("guard fixture parses");
    // `Working` is neither terminal nor in Work's guard, so the only thing
    // that can flag this row is the transition check reading the guard.
    let rows = vec![TursoTrajectoryRow {
        entity_type: "Ticket".to_string(),
        ..row("Work", Some("Working"), Some("Working"))
    }];

    let report = check_conformance(&automaton, &rows, None);

    let violation = only_violation(&report);
    assert_eq!(violation.index, 0);
    assert_eq!(violation.kind, ViolationKind::IllegalTransition);
    assert!(
        violation.detail.contains("Open"),
        "the guard's values are the legal source set: {}",
        violation.detail
    );
}

#[test]
fn a_guarded_action_keeps_its_source_states_out_of_the_terminal_set() {
    let automaton = parse_automaton(GUARD_ONLY_IOA).expect("guard fixture parses");
    // `Open` is listed in no action's `from`; only Work's `state_in` guard
    // names it. Reading `from` alone would call it terminal.
    let rows = vec![TursoTrajectoryRow {
        entity_type: "Ticket".to_string(),
        ..row("Work", Some("Open"), Some("Working"))
    }];

    let report = check_conformance(&automaton, &rows, None);

    assert!(report.passed, "violations: {:?}", report.violations);
    assert_eq!(report.stats.terminal_entities, 0);
}

#[test]
fn forbidden_action_flags_a_platform_action_the_actor_does_not_declare() {
    let rows = vec![
        row("AddItem", Some("Draft"), Some("Draft")),
        // `Created` is a name the kernel emits; the Order spec never declares
        // it, and this row came through the entity dispatch path.
        row("Created", Some("Draft"), Some("Draft")),
    ];

    let report = check_conformance(&order_automaton(), &rows, None);

    let violation = only_violation(&report);
    assert_eq!(violation.index, 1);
    assert_eq!(violation.kind, ViolationKind::ForbiddenAction);
    assert_eq!(violation.action, "Created");
    assert!(violation.detail.contains("platform defines"));
}

#[test]
fn unknown_action_flags_a_name_no_spec_defines() {
    let rows = vec![
        row("AddItem", Some("Draft"), Some("Draft")),
        row("Frobnicate", Some("Draft"), None),
    ];

    let report = check_conformance(&order_automaton(), &rows, None);

    let violation = only_violation(&report);
    assert_eq!(violation.index, 1);
    assert_eq!(violation.kind, ViolationKind::UnknownAction);
    assert_eq!(violation.action, "Frobnicate");
    assert!(violation.detail.contains("no `[[action]]`"));
}

#[test]
fn post_terminal_flags_every_action_after_the_terminal_transition() {
    let rows = vec![
        row("CancelOrder", Some("Draft"), Some("Cancelled")),
        row("AddItem", Some("Cancelled"), Some("Cancelled")),
    ];

    let report = check_conformance(&order_automaton(), &rows, None);

    let violation = only_violation(&report);
    assert_eq!(violation.index, 1);
    assert_eq!(violation.kind, ViolationKind::PostTerminal);
    assert_eq!(violation.action, "AddItem");
    assert!(
        violation.detail.contains("index 0"),
        "detail must point back at the terminal transition: {}",
        violation.detail
    );
    assert_eq!(report.stats.terminal_entities, 1);
    assert!(
        !report
            .stats
            .violations_by_kind
            .contains_key("illegal_transition"),
        "post_terminal suppresses the redundant illegal_transition on the same row"
    );
}

#[test]
fn post_terminal_is_caught_when_the_session_starts_after_the_terminal_transition() {
    // No row in this session drove the entity into Cancelled; the source state
    // alone has to be enough.
    let rows = vec![row("AddItem", Some("Cancelled"), Some("Cancelled"))];

    let report = check_conformance(&order_automaton(), &rows, None);

    let violation = only_violation(&report);
    assert_eq!(violation.index, 0);
    assert_eq!(violation.kind, ViolationKind::PostTerminal);
    assert!(violation.detail.contains("Cancelled"));
}

#[test]
fn denied_then_retried_flags_a_blind_retry() {
    let rows = vec![
        denied("SubmitOrder", Some("Draft")),
        failed("SubmitOrder", Some("Draft")),
    ];

    let report = check_conformance(&order_automaton(), &rows, None);

    let violation = only_violation(&report);
    assert_eq!(violation.index, 1);
    assert_eq!(violation.kind, ViolationKind::DeniedThenRetried);
    assert_eq!(violation.action, "SubmitOrder");
    assert!(
        violation.detail.contains("index 0"),
        "detail must point back at the denial: {}",
        violation.detail
    );
}

#[test]
fn a_retry_after_a_state_change_is_not_a_violation() {
    let rows = vec![
        denied("CancelOrder", Some("Draft")),
        row("SubmitOrder", Some("Draft"), Some("Submitted")),
        failed("CancelOrder", Some("Submitted")),
    ];

    let report = check_conformance(&order_automaton(), &rows, None);

    assert!(
        report.passed,
        "the entity moved between denial and retry: {:?}",
        report.violations
    );
}

#[test]
fn a_retry_that_succeeds_is_not_a_violation() {
    let rows = vec![
        denied("SubmitOrder", Some("Draft")),
        row("SubmitOrder", Some("Draft"), Some("Submitted")),
    ];

    let report = check_conformance(&order_automaton(), &rows, None);

    assert!(
        report.passed,
        "authorization allowed the retry, so an approval landed: {:?}",
        report.violations
    );
}

#[test]
fn a_denial_on_a_different_entity_does_not_arm_the_retry_check() {
    let rows = vec![
        denied("SubmitOrder", Some("Draft")),
        TursoTrajectoryRow {
            entity_id: "order-2".to_string(),
            ..failed("SubmitOrder", Some("Draft"))
        },
    ];

    let report = check_conformance(&order_automaton(), &rows, None);

    assert!(report.passed, "violations: {:?}", report.violations);
}

#[test]
fn three_denials_report_each_blind_retry() {
    let rows = vec![
        denied("SubmitOrder", Some("Draft")),
        denied("SubmitOrder", Some("Draft")),
        denied("SubmitOrder", Some("Draft")),
    ];

    let report = check_conformance(&order_automaton(), &rows, None);

    let indices: Vec<usize> = report.violations.iter().map(|v| v.index).collect();
    assert_eq!(indices, vec![1, 2]);
    assert!(
        report
            .violations
            .iter()
            .all(|v| v.kind == ViolationKind::DeniedThenRetried)
    );
    assert_eq!(report.stats.violations_by_kind["denied_then_retried"], 2);
}

#[test]
fn platform_rows_and_other_entities_are_counted_but_not_judged() {
    let rows = vec![
        // A platform-sourced bookkeeping row naming an action Order never
        // declares: kernel bookkeeping, not an actor action.
        TursoTrajectoryRow {
            source: Some("Platform".to_string()),
            ..row("EntitySetNotFound", None, None)
        },
        // Another actor's row; this checker was given only the Order spec.
        TursoTrajectoryRow {
            entity_type: "Invoice".to_string(),
            ..row("Frobnicate", Some("Nowhere"), None)
        },
        row("AddItem", Some("Draft"), Some("Draft")),
    ];

    let report = check_conformance(&order_automaton(), &rows, None);

    assert!(report.passed, "violations: {:?}", report.violations);
    assert_eq!(report.stats.stream_length, 3);
    assert_eq!(report.stats.actor_rows, 1);
    assert_eq!(report.stats.platform_rows_skipped, 1);
    assert_eq!(report.stats.other_entity_rows_skipped, 1);
}

#[test]
fn a_row_without_a_source_state_is_counted_as_unchecked() {
    let rows = vec![row("ShipOrder", None, Some("Shipped"))];

    let report = check_conformance(&order_automaton(), &rows, None);

    assert!(report.passed, "violations: {:?}", report.violations);
    assert_eq!(report.stats.transitions_unchecked, 1);
}

fn ots_with_decisions(actions: &[&str]) -> OTSTrajectory {
    let now = "2026-01-01T00:00:00Z"
        .parse::<chrono::DateTime<chrono::Utc>>()
        .expect("timestamp parses");
    let mut turn = OTSTurn::new(1, now);
    for action in actions {
        turn = turn.with_decision(OTSDecision::new(
            DecisionType::ToolSelection,
            OTSChoice::new(*action),
            OTSConsequence::failure(),
        ));
    }
    OTSTrajectory::new(OTSMetadata::new(
        "task",
        "agent-1",
        OutcomeType::Failure,
        now,
    ))
    .with_turn(turn)
}

#[test]
fn ots_decisions_the_kernel_never_recorded_are_checked_after_the_rows() {
    let rows = vec![row("AddItem", Some("Draft"), Some("Draft"))];
    // `AddItem` already has a row and must not be double-counted; `Frobnicate`
    // never reached the kernel at all.
    let ots = ots_with_decisions(&["AddItem", "Frobnicate"]);

    let report = check_conformance(&order_automaton(), &rows, Some(&ots));

    let violation = only_violation(&report);
    assert_eq!(
        violation.index, 1,
        "OTS decisions are indexed after the kernel rows"
    );
    assert_eq!(violation.kind, ViolationKind::UnknownAction);
    assert_eq!(violation.action, "Frobnicate");
    assert!(violation.detail.contains("never recorded a row"));
    assert_eq!(report.stats.ots_decisions_checked, 1);
    assert_eq!(report.stats.stream_length, 2);
}

#[test]
fn ots_decisions_on_declared_actions_raise_nothing() {
    let rows = vec![row("AddItem", Some("Draft"), Some("Draft"))];
    let ots = ots_with_decisions(&["CancelOrder"]);

    let report = check_conformance(&order_automaton(), &rows, Some(&ots));

    assert!(report.passed, "violations: {:?}", report.violations);
    assert_eq!(report.stats.ots_decisions_checked, 1);
}

#[test]
fn an_empty_session_passes_with_an_empty_stream() {
    let report = check_conformance(&order_automaton(), &[], None);

    assert!(report.passed);
    assert_eq!(report.stats.stream_length, 0);
    assert_eq!(report.stats.actor_rows, 0);
}
