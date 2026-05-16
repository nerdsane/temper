use std::collections::{HashMap, HashSet};
use std::str::FromStr;

use cedar_policy::{
    Authorizer, Context, Decision, Entities, Entity, EntityUid, PolicySet, Request,
};

use super::*;

fn uid(value: &str) -> EntityUid {
    EntityUid::from_str(value).expect("test UID should parse")
}

fn policy_set(source: &str) -> PolicySet {
    PolicySet::from_str(source).expect("test policy should parse")
}

fn decision_and_reasons(
    policy_set: &PolicySet,
    principal_uid: &EntityUid,
    action_uid: &EntityUid,
    resource_uid: &EntityUid,
) -> (Decision, Vec<String>) {
    let principal = Entity::new(principal_uid.clone(), HashMap::new(), HashSet::new())
        .expect("principal entity should build");
    let resource = Entity::new(resource_uid.clone(), HashMap::new(), HashSet::new())
        .expect("resource entity should build");
    let entities =
        Entities::from_entities([principal, resource], None).expect("entities should build");
    let request = Request::new(
        principal_uid.clone(),
        action_uid.clone(),
        resource_uid.clone(),
        Context::empty(),
        None,
    )
    .expect("request should build");
    let response = Authorizer::new().is_authorized(&request, policy_set, &entities);
    let reasons = response
        .diagnostics()
        .reason()
        .map(|id| id.to_string())
        .collect();
    (response.decision(), reasons)
}

fn filtered_policy_set(
    policy_set: &PolicySet,
    principal_uid: &EntityUid,
    action_uid: &EntityUid,
    resource_uid: &EntityUid,
) -> (PolicySet, CandidatePolicyCounts) {
    let selection =
        select_candidate_policy_set(policy_set, principal_uid, action_uid, resource_uid);
    (
        selection.policy_set.unwrap_or_else(|| policy_set.clone()),
        selection.counts,
    )
}

fn indexed_policy_set(
    policy_set: &PolicySet,
    principal_uid: &EntityUid,
    action_uid: &EntityUid,
    resource_uid: &EntityUid,
) -> (PolicySet, CandidatePolicyCounts) {
    let index = CandidatePolicyIndex::new(policy_set);
    let selection = index.select(principal_uid, action_uid, resource_uid);
    (
        selection.policy_set.unwrap_or_else(|| policy_set.clone()),
        selection.counts,
    )
}

#[test]
fn filters_impossible_scopes_without_changing_allow_diagnostics() {
    let policies = policy_set(
        r#"
        @id("permit-doc-read")
        permit(principal is Customer, action == Action::"read", resource is Doc);

        @id("irrelevant-action")
        permit(principal is Customer, action == Action::"write", resource is Doc);

        @id("irrelevant-principal")
        permit(principal is Agent, action == Action::"read", resource is Doc);

        @id("irrelevant-resource")
        permit(principal is Customer, action == Action::"read", resource is Issue);

        @id("irrelevant-resource-id")
        forbid(principal is Customer, action == Action::"read", resource == Doc::"blocked");
        "#,
    );
    let principal = uid(r#"Customer::"alice""#);
    let action = uid(r#"Action::"read""#);
    let resource = uid(r#"Doc::"doc-1""#);

    let (filtered, counts) = filtered_policy_set(&policies, &principal, &action, &resource);

    assert_eq!(counts.full, 5);
    assert_eq!(counts.candidate, 1);
    assert_eq!(counts.outcome, CandidateSelectionOutcome::Filtered);
    assert_eq!(
        decision_and_reasons(&policies, &principal, &action, &resource),
        decision_and_reasons(&filtered, &principal, &action, &resource)
    );
}

#[test]
fn indexed_selection_matches_scan_selection_and_diagnostics() {
    let policies = policy_set(
        r#"
        @id("permit-doc-read")
        permit(principal is Customer, action == Action::"read", resource is Doc);

        @id("permit-doc-read-by-id")
        permit(principal == Customer::"alice", action in [Action::"read", Action::"list"], resource == Doc::"doc-1");

        @id("forbid-doc-1")
        forbid(principal is Customer, action == Action::"read", resource == Doc::"doc-1");

        @id("broad-resource-tree")
        permit(principal in Group::"customers", action == Action::"read", resource in Folder::"docs");

        @id("irrelevant-principal")
        permit(principal is Agent, action == Action::"read", resource is Doc);

        @id("irrelevant-action")
        permit(principal is Customer, action == Action::"write", resource is Doc);

        @id("irrelevant-resource")
        permit(principal is Customer, action == Action::"read", resource is Issue);
        "#,
    );
    let principal = uid(r#"Customer::"alice""#);
    let action = uid(r#"Action::"read""#);
    let resource = uid(r#"Doc::"doc-1""#);

    let (scan_filtered, scan_counts) =
        filtered_policy_set(&policies, &principal, &action, &resource);
    let (indexed_filtered, indexed_counts) =
        indexed_policy_set(&policies, &principal, &action, &resource);

    assert_eq!(scan_counts, indexed_counts);
    assert_eq!(
        decision_and_reasons(&scan_filtered, &principal, &action, &resource),
        decision_and_reasons(&indexed_filtered, &principal, &action, &resource)
    );
    assert_eq!(
        decision_and_reasons(&policies, &principal, &action, &resource),
        decision_and_reasons(&indexed_filtered, &principal, &action, &resource)
    );
}

#[test]
fn indexed_selection_matches_scan_for_large_policy_sets() {
    let mut source = String::new();
    for idx in 0..1_000 {
        source.push_str(&format!(
            r#"
            @id("irrelevant-{idx}")
            permit(principal is Agent, action == Action::"noop-{idx}", resource is Issue);
            "#
        ));
    }
    source.push_str(
        r#"
        @id("permit-doc-read")
        permit(principal is Customer, action == Action::"read", resource is Doc);

        @id("forbid-doc-1")
        forbid(principal is Customer, action == Action::"read", resource == Doc::"doc-1");
        "#,
    );

    let policies = policy_set(&source);
    let principal = uid(r#"Customer::"alice""#);
    let action = uid(r#"Action::"read""#);
    let resource = uid(r#"Doc::"doc-1""#);

    let (scan_filtered, scan_counts) =
        filtered_policy_set(&policies, &principal, &action, &resource);
    let (indexed_filtered, indexed_counts) =
        indexed_policy_set(&policies, &principal, &action, &resource);

    assert_eq!(scan_counts.full, 1_002);
    assert_eq!(scan_counts.candidate, 2);
    assert_eq!(scan_counts, indexed_counts);
    assert_eq!(
        decision_and_reasons(&scan_filtered, &principal, &action, &resource),
        decision_and_reasons(&indexed_filtered, &principal, &action, &resource)
    );
}

#[test]
fn preserves_matching_forbid_deny_override() {
    let policies = policy_set(
        r#"
        @id("permit-doc-read")
        permit(principal is Customer, action == Action::"read", resource is Doc);

        @id("forbid-blocked-doc")
        forbid(principal is Customer, action == Action::"read", resource == Doc::"blocked");

        @id("irrelevant-resource")
        permit(principal is Customer, action == Action::"read", resource is Issue);
        "#,
    );
    let principal = uid(r#"Customer::"alice""#);
    let action = uid(r#"Action::"read""#);
    let resource = uid(r#"Doc::"blocked""#);

    let (filtered, counts) = filtered_policy_set(&policies, &principal, &action, &resource);
    let full_result = decision_and_reasons(&policies, &principal, &action, &resource);
    let filtered_result = decision_and_reasons(&filtered, &principal, &action, &resource);

    assert_eq!(counts.full, 3);
    assert_eq!(counts.candidate, 2);
    assert_eq!(full_result, filtered_result);
    assert_eq!(filtered_result.0, Decision::Deny);
    assert!(!filtered_result.1.is_empty());
}

#[test]
fn empty_candidate_set_still_denies_without_reasons() {
    let policies = policy_set(
        r#"
        @id("irrelevant-action")
        permit(principal is Customer, action == Action::"write", resource is Doc);

        @id("irrelevant-resource")
        permit(principal is Customer, action == Action::"read", resource is Issue);
        "#,
    );
    let principal = uid(r#"Customer::"alice""#);
    let action = uid(r#"Action::"read""#);
    let resource = uid(r#"Doc::"doc-1""#);

    let (filtered, counts) = filtered_policy_set(&policies, &principal, &action, &resource);

    assert_eq!(counts.full, 2);
    assert_eq!(counts.candidate, 0);
    assert_eq!(
        decision_and_reasons(&policies, &principal, &action, &resource),
        decision_and_reasons(&filtered, &principal, &action, &resource)
    );
    assert_eq!(
        decision_and_reasons(&filtered, &principal, &action, &resource),
        (Decision::Deny, Vec::new())
    );
}

#[test]
fn broad_forbids_remain_candidates() {
    let policies = policy_set(
        r#"
        @id("permit-doc-read")
        permit(principal is Customer, action == Action::"read", resource is Doc);

        @id("broad-forbid")
        forbid(principal, action, resource);

        @id("irrelevant-action")
        permit(principal is Customer, action == Action::"write", resource is Doc);
        "#,
    );
    let principal = uid(r#"Customer::"alice""#);
    let action = uid(r#"Action::"read""#);
    let resource = uid(r#"Doc::"doc-1""#);

    let (filtered, counts) = filtered_policy_set(&policies, &principal, &action, &resource);
    let filtered_result = decision_and_reasons(&filtered, &principal, &action, &resource);

    assert_eq!(counts.full, 3);
    assert_eq!(counts.candidate, 2);
    assert_eq!(
        decision_and_reasons(&policies, &principal, &action, &resource),
        filtered_result
    );
    assert_eq!(filtered_result.0, Decision::Deny);
    assert!(!filtered_result.1.is_empty());
}

#[test]
fn hierarchy_constraints_are_included_conservatively() {
    let policies = policy_set(
        r#"
        @id("principal-hierarchy")
        permit(principal in Group::"operators", action == Action::"read", resource is Doc);

        @id("resource-hierarchy")
        permit(principal is Customer, action == Action::"read", resource in Folder::"root");

        @id("irrelevant-resource")
        permit(principal is Customer, action == Action::"read", resource is Issue);
        "#,
    );
    let principal = uid(r#"Customer::"alice""#);
    let action = uid(r#"Action::"read""#);
    let resource = uid(r#"Doc::"doc-1""#);

    let (_, counts) = filtered_policy_set(&policies, &principal, &action, &resource);

    assert_eq!(counts.full, 3);
    assert_eq!(counts.candidate, 2);
    assert_eq!(counts.outcome, CandidateSelectionOutcome::Filtered);
}
