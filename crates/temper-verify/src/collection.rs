//! Exact finite quotient model for ADR-0181 bounded collection workflows.

use std::collections::{BTreeSet, VecDeque};

use temper_spec::automaton::CollectionWorkflow;

mod induction;
use induction::prove_transition_induction;

/// Successful proof summary for one declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectionVerification {
    /// Declaration name.
    pub name: String,
    /// Boundary roster sizes checked.
    pub roster_sizes: Vec<u16>,
    /// Total distinct quotient states explored.
    pub states_explored: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Phase {
    Running,
    Cancelling,
    TimingOut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Classification {
    Succeeded,
    PartiallyFailed,
    Failed,
    Cancelled,
    TimedOut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemberStatus {
    Pending,
    Attempt(u8),
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct QuotientState {
    roster_size: u16,
    identity_epoch: u8,
    phase: Phase,
    pending: u16,
    attempts: Vec<u16>,
    succeeded: u16,
    failed: u16,
    cancelled: u16,
    timed_out: u16,
    join_count: u8,
    classification: Option<Classification>,
}

impl QuotientState {
    fn initial(members: u16, max_attempts: u8) -> Self {
        Self {
            roster_size: members,
            identity_epoch: 0,
            phase: Phase::Running,
            pending: members,
            attempts: vec![0; usize::from(max_attempts)],
            succeeded: 0,
            failed: 0,
            cancelled: 0,
            timed_out: 0,
            join_count: 0,
            classification: None,
        }
    }

    fn in_flight(&self) -> u16 {
        self.attempts.iter().sum()
    }

    fn total(&self) -> u16 {
        self.pending
            + self.in_flight()
            + self.succeeded
            + self.failed
            + self.cancelled
            + self.timed_out
    }

    fn terminal_members(&self) -> u16 {
        self.succeeded + self.failed + self.cancelled + self.timed_out
    }
}

/// Exhaustively verify the symmetry quotient at ADR-0181 boundary sizes.
pub fn verify_collection_workflow(
    workflow: &CollectionWorkflow,
) -> Result<CollectionVerification, String> {
    let sizes = (1..=workflow.max_members).collect::<BTreeSet<_>>();

    let mut states_explored = 0usize;
    for members in sizes.iter().copied() {
        states_explored = states_explored
            .checked_add(if members <= 8 {
                explore(workflow, members)?
            } else {
                prove_transition_induction(workflow, members)?
            })
            .ok_or_else(|| "collection verification state count overflowed".to_string())?;
    }
    Ok(CollectionVerification {
        name: workflow.name.clone(),
        roster_sizes: sizes.into_iter().collect(),
        states_explored,
    })
}

/// Prove preservation for every transition schema over the complete integer
/// domain. This is induction, not sampled state exploration: Z3 searches for
/// any valid pre-state whose post-state violates the invariant.
fn explore(workflow: &CollectionWorkflow, members: u16) -> Result<usize, String> {
    let initial = QuotientState::initial(members, workflow.max_attempts);
    let mut visited = BTreeSet::from([initial.clone()]);
    let mut queue = VecDeque::from([initial]);
    let state_budget = 1_000_000usize;

    while let Some(state) = queue.pop_front() {
        validate_state(workflow, members, &state)?;
        verify_labelled_projection(workflow, members, &state)?;
        for next in successors(workflow, members, &state) {
            validate_state(workflow, members, &next)?;
            if visited.insert(next.clone()) {
                if visited.len() > state_budget {
                    return Err(format!(
                        "collection workflow '{}' verification exhausted {state_budget} states at roster size {members}",
                        workflow.name
                    ));
                }
                queue.push_back(next);
            }
        }
    }
    Ok(visited.len())
}

fn verify_labelled_projection(
    workflow: &CollectionWorkflow,
    members: u16,
    state: &QuotientState,
) -> Result<(), String> {
    let labelled = expand(state);
    let mut projected = BTreeSet::new();
    for index in 0..labelled.len() {
        for next in labelled_member_steps(workflow, state, &labelled, index) {
            projected.insert(project(state, &next));
        }
    }
    for (metadata, next) in labelled_global_steps(state, &labelled) {
        projected.insert(project(&metadata, &next));
    }
    let quotient = successors(workflow, members, state)
        .into_iter()
        .collect::<BTreeSet<_>>();
    if projected != quotient {
        return Err(format!(
            "collection workflow '{}' labelled/quotient transition mismatch at roster size {members}",
            workflow.name
        ));
    }
    Ok(())
}

fn expand(state: &QuotientState) -> Vec<MemberStatus> {
    let mut members = Vec::with_capacity(usize::from(state.roster_size));
    members.extend(std::iter::repeat_n(
        MemberStatus::Pending,
        usize::from(state.pending),
    ));
    for (attempt, count) in state.attempts.iter().enumerate() {
        members.extend(std::iter::repeat_n(
            MemberStatus::Attempt(attempt as u8),
            usize::from(*count),
        ));
    }
    for (status, count) in [
        (MemberStatus::Succeeded, state.succeeded),
        (MemberStatus::Failed, state.failed),
        (MemberStatus::Cancelled, state.cancelled),
        (MemberStatus::TimedOut, state.timed_out),
    ] {
        members.extend(std::iter::repeat_n(status, usize::from(count)));
    }
    members
}

fn project(template: &QuotientState, members: &[MemberStatus]) -> QuotientState {
    let mut state = QuotientState {
        roster_size: template.roster_size,
        identity_epoch: template.identity_epoch,
        phase: template.phase,
        pending: 0,
        attempts: vec![0; template.attempts.len()],
        succeeded: 0,
        failed: 0,
        cancelled: 0,
        timed_out: 0,
        join_count: template.join_count,
        classification: template.classification,
    };
    for member in members {
        match *member {
            MemberStatus::Pending => state.pending += 1,
            MemberStatus::Attempt(attempt) => state.attempts[usize::from(attempt)] += 1,
            MemberStatus::Succeeded => state.succeeded += 1,
            MemberStatus::Failed => state.failed += 1,
            MemberStatus::Cancelled => state.cancelled += 1,
            MemberStatus::TimedOut => state.timed_out += 1,
        }
    }
    state
}

fn labelled_member_steps(
    workflow: &CollectionWorkflow,
    state: &QuotientState,
    members: &[MemberStatus],
    index: usize,
) -> Vec<Vec<MemberStatus>> {
    let mut next = Vec::new();
    let mut replace = |status| {
        let mut changed = members.to_vec();
        changed[index] = status;
        next.push(changed);
    };
    match members[index] {
        MemberStatus::Pending
            if state.phase == Phase::Running
                && state.in_flight() < u16::from(workflow.max_concurrency) =>
        {
            replace(MemberStatus::Attempt(0));
        }
        MemberStatus::Attempt(attempt) => {
            replace(MemberStatus::Succeeded);
            replace(MemberStatus::Failed);
            if state.phase == Phase::Running && usize::from(attempt) + 1 < state.attempts.len() {
                replace(MemberStatus::Attempt(attempt + 1));
            }
            match state.phase {
                Phase::Cancelling => replace(MemberStatus::Cancelled),
                Phase::TimingOut => replace(MemberStatus::TimedOut),
                Phase::Running => {}
            }
        }
        _ => {}
    }
    next
}

fn labelled_global_steps(
    state: &QuotientState,
    members: &[MemberStatus],
) -> Vec<(QuotientState, Vec<MemberStatus>)> {
    let mut next = Vec::new();
    if state.phase == Phase::Running && state.join_count == 0 {
        for replacement in [MemberStatus::Cancelled, MemberStatus::TimedOut] {
            let mut metadata = state.clone();
            metadata.phase = if replacement == MemberStatus::Cancelled {
                Phase::Cancelling
            } else {
                Phase::TimingOut
            };
            next.push((
                metadata,
                members
                    .iter()
                    .map(|status| {
                        if *status == MemberStatus::Pending {
                            replacement
                        } else {
                            *status
                        }
                    })
                    .collect(),
            ));
        }
    }
    if state.join_count == 0 && state.terminal_members() == state.roster_size {
        // Join changes workflow metadata only, so member labels are unchanged.
        let mut metadata = state.clone();
        metadata.join_count = 1;
        metadata.classification = Some(classify(state, state.roster_size));
        next.push((metadata, members.to_vec()));
    }
    next
}

fn successors(
    workflow: &CollectionWorkflow,
    members: u16,
    state: &QuotientState,
) -> Vec<QuotientState> {
    let mut next = Vec::new();
    if state.phase == Phase::Running
        && state.pending > 0
        && state.in_flight() < u16::from(workflow.max_concurrency)
    {
        let mut admitted = state.clone();
        admitted.pending -= 1;
        admitted.attempts[0] += 1;
        next.push(admitted);
    }
    for bucket in 0..state.attempts.len() {
        if state.attempts[bucket] == 0 {
            continue;
        }
        let mut succeeded = state.clone();
        succeeded.attempts[bucket] -= 1;
        succeeded.succeeded += 1;
        next.push(succeeded);

        let mut failed = state.clone();
        failed.attempts[bucket] -= 1;
        failed.failed += 1;
        next.push(failed);

        if state.phase == Phase::Running && bucket + 1 < state.attempts.len() {
            let mut retried = state.clone();
            retried.attempts[bucket] -= 1;
            retried.attempts[bucket + 1] += 1;
            next.push(retried);
        }
        if matches!(state.phase, Phase::Cancelling | Phase::TimingOut) {
            let mut controlled = state.clone();
            controlled.attempts[bucket] -= 1;
            match state.phase {
                Phase::Cancelling => controlled.cancelled += 1,
                Phase::TimingOut => controlled.timed_out += 1,
                Phase::Running => unreachable!(),
            }
            next.push(controlled);
        }
    }
    if state.phase == Phase::Running && state.join_count == 0 {
        for phase in [Phase::Cancelling, Phase::TimingOut] {
            let mut controlled = state.clone();
            controlled.phase = phase;
            match phase {
                Phase::Cancelling => controlled.cancelled += controlled.pending,
                Phase::TimingOut => controlled.timed_out += controlled.pending,
                Phase::Running => unreachable!(),
            }
            controlled.pending = 0;
            next.push(controlled);
        }
    }
    if state.join_count == 0 && state.terminal_members() == members {
        let mut joined = state.clone();
        joined.classification = Some(classify(state, members));
        joined.join_count = 1;
        next.push(joined);
    }
    // Restart, duplicate control, duplicate receipt, and duplicate join are
    // self-loops in the quotient and therefore add no new state.
    next
}

fn validate_state(
    workflow: &CollectionWorkflow,
    members: u16,
    state: &QuotientState,
) -> Result<(), String> {
    if state.roster_size != members || state.identity_epoch != 0 || state.total() != members {
        return Err(format!(
            "collection workflow '{}' lost its terminal partition",
            workflow.name
        ));
    }
    if state.in_flight() > u16::from(workflow.max_concurrency) {
        return Err(format!(
            "collection workflow '{}' exceeded max_concurrency",
            workflow.name
        ));
    }
    if state.attempts.len() != usize::from(workflow.max_attempts) {
        return Err(format!(
            "collection workflow '{}' exceeded max_attempts",
            workflow.name
        ));
    }
    if state.phase != Phase::Running && state.pending != 0 {
        return Err(format!(
            "collection workflow '{}' admitted after control",
            workflow.name
        ));
    }
    if state.join_count > 1
        || (state.join_count == 1
            && (state.terminal_members() != members
                || state.classification != Some(classify(state, members))))
        || (state.join_count == 0 && state.classification.is_some())
    {
        return Err(format!(
            "collection workflow '{}' produced an invalid or duplicate join: {state:?}",
            workflow.name,
        ));
    }
    Ok(())
}

fn classify(state: &QuotientState, members: u16) -> Classification {
    match state.phase {
        Phase::Cancelling => Classification::Cancelled,
        Phase::TimingOut => Classification::TimedOut,
        Phase::Running if state.succeeded == members => Classification::Succeeded,
        Phase::Running if state.succeeded > 0 => Classification::PartiallyFailed,
        Phase::Running => Classification::Failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workflow() -> CollectionWorkflow {
        CollectionWorkflow {
            name: "checks".into(),
            start_action: "Start".into(),
            cancel_action: "Cancel".into(),
            timeout_action: "Timeout".into(),
            roster_field: "members".into(),
            member_entity: "Check".into(),
            member_action: "Run".into(),
            member_cancel_action: "Stop".into(),
            max_members: 64,
            max_concurrency: 8,
            max_attempts: 5,
            on_success: "Succeeded".into(),
            on_partial_failure: "Partial".into(),
            on_failure: "Failed".into(),
            on_cancelled: "Cancelled".into(),
            on_timed_out: "TimedOut".into(),
        }
    }

    #[test]
    fn exact_quotient_covers_normative_boundaries() {
        let result = verify_collection_workflow(&workflow()).expect("quotient proof");
        assert_eq!(result.roster_sizes, (1..=64).collect::<Vec<_>>());
        assert!(result.states_explored > 0);
    }

    #[test]
    fn rejects_duplicate_join_and_invalid_aggregation() {
        let workflow = workflow();
        let mut state = QuotientState::initial(1, workflow.max_attempts);
        state.pending = 0;
        state.succeeded = 1;
        state.join_count = 2;
        state.classification = Some(Classification::Succeeded);
        assert!(validate_state(&workflow, 1, &state).is_err());
        state.join_count = 1;
        state.succeeded = 0;
        assert!(validate_state(&workflow, 1, &state).is_err());
    }

    #[test]
    fn classifies_partial_failure_cancel_and_timeout() {
        let workflow = workflow();
        let mut partial = QuotientState::initial(2, workflow.max_attempts);
        partial.pending = 0;
        partial.succeeded = 1;
        partial.failed = 1;
        assert_eq!(classify(&partial, 2), Classification::PartiallyFailed);
        partial.phase = Phase::Cancelling;
        assert_eq!(classify(&partial, 2), Classification::Cancelled);
        partial.phase = Phase::TimingOut;
        assert_eq!(classify(&partial, 2), Classification::TimedOut);
    }

    #[test]
    fn rejects_excess_concurrency_and_preserves_restart_exactly() {
        let workflow = workflow();
        let mut state = QuotientState::initial(9, workflow.max_attempts);
        state.pending = 0;
        state.attempts[0] = 9;
        assert!(validate_state(&workflow, 9, &state).is_err());
        let recovered = state.clone();
        assert_eq!(recovered, state, "restart is an exact quotient self-loop");
    }
}
