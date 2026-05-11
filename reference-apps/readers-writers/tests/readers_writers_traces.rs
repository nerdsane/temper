use std::collections::BTreeSet;

use readers_writers_reference::{
    ProtocolAction, RequestKind, RwState, TransitionKind, WaitRequest, apply_protocol_action,
    callback_params, proposal_params, validate_proposal,
};
use serde_json::json;

fn fields_from_state(state: &RwState) -> serde_json::Value {
    let readers: Vec<_> = state.readers.iter().copied().collect();
    let writers: Vec<_> = state.writers.iter().copied().collect();
    let waiting: Vec<_> = state
        .waiting
        .iter()
        .map(|req| json!({"kind": req.kind.as_str(), "actor": req.actor}))
        .collect();
    json!({
        "readers": readers,
        "writers": writers,
        "waiting": waiting,
    })
}

#[test]
fn trace_matches_quint_fifo_scenario() {
    let mut state = RwState::default();

    state = apply_protocol_action(&state, &ProtocolAction::TryRead(1))
        .unwrap()
        .after;
    state = apply_protocol_action(&state, &ProtocolAction::TryWrite(2))
        .unwrap()
        .after;
    state = apply_protocol_action(&state, &ProtocolAction::TryRead(3))
        .unwrap()
        .after;

    assert_eq!(
        state.waiting,
        vec![
            WaitRequest {
                kind: RequestKind::Read,
                actor: 1,
            },
            WaitRequest {
                kind: RequestKind::Write,
                actor: 2,
            },
            WaitRequest {
                kind: RequestKind::Read,
                actor: 3,
            },
        ]
    );

    let first = apply_protocol_action(&state, &ProtocolAction::ReadOrWrite).unwrap();
    assert_eq!(first.kind, TransitionKind::ReaderStartedFromIdle);
    assert_eq!(first.after.readers, BTreeSet::from([1]));
    assert_eq!(first.after.waiting[0].actor, 2);

    let blocked = apply_protocol_action(&first.after, &ProtocolAction::ReadOrWrite).unwrap_err();
    assert!(blocked.contains("blocked by readers"));

    let stopped = apply_protocol_action(&first.after, &ProtocolAction::Stop(1)).unwrap();
    assert_eq!(stopped.kind, TransitionKind::ReaderStoppedLast);
    assert_eq!(stopped.after.status(), "Idle");

    let writer = apply_protocol_action(&stopped.after, &ProtocolAction::ReadOrWrite).unwrap();
    assert_eq!(writer.kind, TransitionKind::WriterStarted);
    assert_eq!(writer.after.writers, BTreeSet::from([2]));
    assert_eq!(writer.after.waiting[0].actor, 3);

    let writer_stopped = apply_protocol_action(&writer.after, &ProtocolAction::Stop(2)).unwrap();
    assert_eq!(writer_stopped.kind, TransitionKind::WriterStopped);

    let final_reader =
        apply_protocol_action(&writer_stopped.after, &ProtocolAction::ReadOrWrite).unwrap();
    assert_eq!(final_reader.kind, TransitionKind::ReaderStartedFromIdle);
    assert_eq!(final_reader.after.readers, BTreeSet::from([3]));
    assert!(final_reader.after.waiting.is_empty());
}

#[test]
fn proposal_validation_rejects_counterexample_shape() {
    let before = RwState {
        readers: BTreeSet::from([1]),
        writers: BTreeSet::new(),
        waiting: vec![WaitRequest {
            kind: RequestKind::Write,
            actor: 2,
        }],
    };
    let fields = fields_from_state(&before);
    let forged = json!({
        "proposed_readers": [1],
        "proposed_writers": [2],
        "proposed_waiting": [],
        "last_step": "ReadOrWrite",
    });

    let err = validate_proposal(&fields, &forged).unwrap_err();
    assert!(err.contains("blocked by readers"));
}

#[test]
fn proposal_validation_accepts_exact_quint_step() {
    let before = RwState {
        readers: BTreeSet::new(),
        writers: BTreeSet::new(),
        waiting: vec![WaitRequest {
            kind: RequestKind::Write,
            actor: 2,
        }],
    };
    let action = ProtocolAction::ReadOrWrite;
    let outcome = apply_protocol_action(&before, &action).unwrap();
    let proposal = proposal_params(&outcome, &action);
    let validated = validate_proposal(&fields_from_state(&before), &proposal).unwrap();

    assert_eq!(validated.kind, TransitionKind::WriterStarted);
    assert_eq!(callback_params(&validated, &action)["writers"], json!([2]));
}
