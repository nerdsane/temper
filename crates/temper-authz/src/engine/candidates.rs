use cedar_policy::{
    ActionConstraint, EntityTypeName, EntityUid, Policy, PolicySet, PrincipalConstraint,
    ResourceConstraint,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CandidateSelectionOutcome {
    Full,
    Filtered,
    Fallback,
}

impl CandidateSelectionOutcome {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Filtered => "filtered",
            Self::Fallback => "fallback",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CandidatePolicyCounts {
    pub(super) full: usize,
    pub(super) candidate: usize,
    pub(super) outcome: CandidateSelectionOutcome,
}

#[derive(Debug)]
pub(super) struct CandidatePolicySelection {
    pub(super) policy_set: Option<PolicySet>,
    pub(super) counts: CandidatePolicyCounts,
}

pub(super) fn select_candidate_policy_set(
    policy_set: &PolicySet,
    principal_uid: &EntityUid,
    action_uid: &EntityUid,
    resource_uid: &EntityUid,
) -> CandidatePolicySelection {
    let full_count = policy_set.policies().count();
    if full_count == 0 {
        return use_full_policy_set(full_count);
    }

    let mut candidates = Vec::new();
    let mut candidate_count = 0usize;

    for policy in policy_set.policies() {
        if !policy.is_static() {
            return fallback_policy_set(full_count, full_count);
        }

        if policy_may_match_request(policy, principal_uid, action_uid, resource_uid) {
            candidate_count += 1;
            candidates.push(policy.clone());
        }
    }

    if candidate_count == full_count {
        return use_full_policy_set(full_count);
    }

    match PolicySet::from_policies(candidates) {
        Ok(filtered) => CandidatePolicySelection {
            policy_set: Some(filtered),
            counts: CandidatePolicyCounts {
                full: full_count,
                candidate: candidate_count,
                outcome: CandidateSelectionOutcome::Filtered,
            },
        },
        Err(_) => fallback_policy_set(full_count, candidate_count),
    }
}

fn use_full_policy_set(full_count: usize) -> CandidatePolicySelection {
    CandidatePolicySelection {
        policy_set: None,
        counts: CandidatePolicyCounts {
            full: full_count,
            candidate: full_count,
            outcome: CandidateSelectionOutcome::Full,
        },
    }
}

fn fallback_policy_set(full_count: usize, candidate_count: usize) -> CandidatePolicySelection {
    CandidatePolicySelection {
        policy_set: None,
        counts: CandidatePolicyCounts {
            full: full_count,
            candidate: candidate_count,
            outcome: CandidateSelectionOutcome::Fallback,
        },
    }
}

fn policy_may_match_request(
    policy: &Policy,
    principal_uid: &EntityUid,
    action_uid: &EntityUid,
    resource_uid: &EntityUid,
) -> bool {
    principal_constraint_may_match(policy.principal_constraint(), principal_uid)
        && action_constraint_may_match(policy.action_constraint(), action_uid)
        && resource_constraint_may_match(policy.resource_constraint(), resource_uid)
}

fn principal_constraint_may_match(
    constraint: PrincipalConstraint,
    principal_uid: &EntityUid,
) -> bool {
    match constraint {
        PrincipalConstraint::Any => true,
        PrincipalConstraint::Eq(expected) => expected == *principal_uid,
        PrincipalConstraint::Is(expected_type) => {
            entity_type_matches(&expected_type, principal_uid)
        }
        PrincipalConstraint::In(_) => true,
        PrincipalConstraint::IsIn(expected_type, _) => {
            entity_type_matches(&expected_type, principal_uid)
        }
    }
}

fn action_constraint_may_match(constraint: ActionConstraint, action_uid: &EntityUid) -> bool {
    match constraint {
        ActionConstraint::Any => true,
        ActionConstraint::Eq(expected) => expected == *action_uid,
        ActionConstraint::In(expected) => expected.iter().any(|candidate| candidate == action_uid),
    }
}

fn resource_constraint_may_match(constraint: ResourceConstraint, resource_uid: &EntityUid) -> bool {
    match constraint {
        ResourceConstraint::Any => true,
        ResourceConstraint::Eq(expected) => expected == *resource_uid,
        ResourceConstraint::Is(expected_type) => entity_type_matches(&expected_type, resource_uid),
        ResourceConstraint::In(_) => true,
        ResourceConstraint::IsIn(expected_type, _) => {
            entity_type_matches(&expected_type, resource_uid)
        }
    }
}

fn entity_type_matches(expected_type: &EntityTypeName, uid: &EntityUid) -> bool {
    uid.type_name() == expected_type
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::str::FromStr;

    use cedar_policy::{Authorizer, Context, Decision, Entities, Entity, Request};

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
}
