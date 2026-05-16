use std::collections::BTreeMap;

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

#[derive(Debug, Clone)]
pub(super) struct CandidatePolicyIndex {
    full_count: usize,
    policies: Vec<Policy>,
    state: CandidatePolicyIndexState,
}

#[derive(Debug, Clone)]
enum CandidatePolicyIndexState {
    Indexed(CandidatePolicyBuckets),
    Fallback,
}

#[derive(Debug, Clone, Default)]
struct CandidatePolicyBuckets {
    principal_any: Vec<usize>,
    principal_by_type: BTreeMap<String, Vec<usize>>,
    principal_by_uid: BTreeMap<String, Vec<usize>>,
    action_any: Vec<usize>,
    action_by_uid: BTreeMap<String, Vec<usize>>,
    resource_any: Vec<usize>,
    resource_by_type: BTreeMap<String, Vec<usize>>,
    resource_by_uid: BTreeMap<String, Vec<usize>>,
}

impl CandidatePolicyIndex {
    pub(super) fn new(policy_set: &PolicySet) -> Self {
        let mut policies = Vec::new();
        let mut buckets = CandidatePolicyBuckets::default();
        let mut fallback = false;

        for (idx, policy) in policy_set.policies().enumerate() {
            policies.push(policy.clone());

            if !policy.is_static() {
                fallback = true;
                continue;
            }

            index_principal_constraint(&mut buckets, idx, policy.principal_constraint());
            index_action_constraint(&mut buckets, idx, policy.action_constraint());
            index_resource_constraint(&mut buckets, idx, policy.resource_constraint());
        }

        let state = if fallback {
            CandidatePolicyIndexState::Fallback
        } else {
            CandidatePolicyIndexState::Indexed(buckets)
        };
        Self {
            full_count: policies.len(),
            policies,
            state,
        }
    }

    pub(super) fn select(
        &self,
        principal_uid: &EntityUid,
        action_uid: &EntityUid,
        resource_uid: &EntityUid,
    ) -> CandidatePolicySelection {
        if self.full_count == 0 {
            return use_full_policy_set(self.full_count);
        }

        let CandidatePolicyIndexState::Indexed(buckets) = &self.state else {
            return fallback_policy_set(self.full_count, self.full_count);
        };

        let principal_candidates = union_sorted_buckets([
            Some(&buckets.principal_any),
            buckets
                .principal_by_type
                .get(&entity_type_key(principal_uid.type_name())),
            buckets.principal_by_uid.get(&entity_uid_key(principal_uid)),
        ]);
        let action_candidates = union_sorted_buckets([
            Some(&buckets.action_any),
            buckets.action_by_uid.get(&entity_uid_key(action_uid)),
            None,
        ]);
        let resource_candidates = union_sorted_buckets([
            Some(&buckets.resource_any),
            buckets
                .resource_by_type
                .get(&entity_type_key(resource_uid.type_name())),
            buckets.resource_by_uid.get(&entity_uid_key(resource_uid)),
        ]);

        let candidate_indices = intersect_dimensions(vec![
            principal_candidates,
            action_candidates,
            resource_candidates,
        ]);
        let candidate_count = candidate_indices.len();

        if candidate_count == self.full_count {
            return use_full_policy_set(self.full_count);
        }

        let candidates: Vec<Policy> = candidate_indices
            .iter()
            .map(|idx| self.policies[*idx].clone())
            .collect();

        match PolicySet::from_policies(candidates) {
            Ok(filtered) => CandidatePolicySelection {
                policy_set: Some(filtered),
                counts: CandidatePolicyCounts {
                    full: self.full_count,
                    candidate: candidate_count,
                    outcome: CandidateSelectionOutcome::Filtered,
                },
            },
            Err(_) => fallback_policy_set(self.full_count, candidate_count),
        }
    }
}

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
fn action_constraint_may_match(constraint: ActionConstraint, action_uid: &EntityUid) -> bool {
    match constraint {
        ActionConstraint::Any => true,
        ActionConstraint::Eq(expected) => expected == *action_uid,
        ActionConstraint::In(expected) => expected.iter().any(|candidate| candidate == action_uid),
    }
}

#[cfg(test)]
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

#[cfg(test)]
fn entity_type_matches(expected_type: &EntityTypeName, uid: &EntityUid) -> bool {
    uid.type_name() == expected_type
}

fn index_principal_constraint(
    buckets: &mut CandidatePolicyBuckets,
    idx: usize,
    constraint: PrincipalConstraint,
) {
    match constraint {
        PrincipalConstraint::Any | PrincipalConstraint::In(_) => {
            push_index(&mut buckets.principal_any, idx);
        }
        PrincipalConstraint::Eq(expected) => {
            push_index(
                buckets
                    .principal_by_uid
                    .entry(entity_uid_key(&expected))
                    .or_default(),
                idx,
            );
        }
        PrincipalConstraint::Is(expected_type) | PrincipalConstraint::IsIn(expected_type, _) => {
            push_index(
                buckets
                    .principal_by_type
                    .entry(entity_type_key(&expected_type))
                    .or_default(),
                idx,
            );
        }
    }
}

fn index_action_constraint(
    buckets: &mut CandidatePolicyBuckets,
    idx: usize,
    constraint: ActionConstraint,
) {
    match constraint {
        ActionConstraint::Any => push_index(&mut buckets.action_any, idx),
        ActionConstraint::Eq(expected) => {
            push_index(
                buckets
                    .action_by_uid
                    .entry(entity_uid_key(&expected))
                    .or_default(),
                idx,
            );
        }
        ActionConstraint::In(expected) => {
            for action_uid in expected {
                push_index(
                    buckets
                        .action_by_uid
                        .entry(entity_uid_key(&action_uid))
                        .or_default(),
                    idx,
                );
            }
        }
    }
}

fn index_resource_constraint(
    buckets: &mut CandidatePolicyBuckets,
    idx: usize,
    constraint: ResourceConstraint,
) {
    match constraint {
        ResourceConstraint::Any | ResourceConstraint::In(_) => {
            push_index(&mut buckets.resource_any, idx);
        }
        ResourceConstraint::Eq(expected) => {
            push_index(
                buckets
                    .resource_by_uid
                    .entry(entity_uid_key(&expected))
                    .or_default(),
                idx,
            );
        }
        ResourceConstraint::Is(expected_type) | ResourceConstraint::IsIn(expected_type, _) => {
            push_index(
                buckets
                    .resource_by_type
                    .entry(entity_type_key(&expected_type))
                    .or_default(),
                idx,
            );
        }
    }
}

fn push_index(bucket: &mut Vec<usize>, idx: usize) {
    if bucket.last().copied() != Some(idx) {
        bucket.push(idx);
    }
}

fn entity_uid_key(uid: &EntityUid) -> String {
    uid.to_string()
}

fn entity_type_key(entity_type: &EntityTypeName) -> String {
    entity_type.to_string()
}

fn union_sorted_buckets<const N: usize>(buckets: [Option<&Vec<usize>>; N]) -> Vec<usize> {
    let mut result = Vec::new();
    for bucket in buckets.into_iter().flatten() {
        result.extend_from_slice(bucket);
    }
    result.sort_unstable();
    result.dedup();
    result
}

fn intersect_dimensions(mut dimensions: Vec<Vec<usize>>) -> Vec<usize> {
    if dimensions.iter().any(Vec::is_empty) {
        return Vec::new();
    }

    dimensions.sort_by_key(Vec::len);
    let mut result = dimensions.remove(0);
    for dimension in dimensions {
        result = intersect_sorted(&result, &dimension);
        if result.is_empty() {
            break;
        }
    }
    result
}

fn intersect_sorted(left: &[usize], right: &[usize]) -> Vec<usize> {
    let mut result = Vec::new();
    let mut left_idx = 0usize;
    let mut right_idx = 0usize;

    while left_idx < left.len() && right_idx < right.len() {
        match left[left_idx].cmp(&right[right_idx]) {
            std::cmp::Ordering::Less => left_idx += 1,
            std::cmp::Ordering::Greater => right_idx += 1,
            std::cmp::Ordering::Equal => {
                result.push(left[left_idx]);
                left_idx += 1;
                right_idx += 1;
            }
        }
    }

    result
}

#[cfg(test)]
#[path = "candidates_tests.rs"]
mod tests;
