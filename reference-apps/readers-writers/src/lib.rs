//! Reference Readers/Writers application for Temper.
//!
//! The pure transition functions in this crate mirror the Quint
//! ReadersWriters example. The WASM module uses the same functions so tests
//! can exercise the transition relation without a WASM host.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const READERS_WRITERS_IOA: &str = include_str!("../specs/readers_writers.ioa.toml");
pub const MODEL_CSDL: &str = include_str!("../specs/model.csdl.xml");
pub const READERS_WRITERS_CEDAR: &str = include_str!("../specs/policies/readers_writers.cedar");

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitRequest {
    pub kind: RequestKind,
    pub actor: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RequestKind {
    Read,
    Write,
}

impl RequestKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RwState {
    pub readers: BTreeSet<i64>,
    pub writers: BTreeSet<i64>,
    pub waiting: Vec<WaitRequest>,
}

impl RwState {
    pub fn status(&self) -> &'static str {
        if !self.writers.is_empty() {
            "Writing"
        } else if !self.readers.is_empty() {
            "Reading"
        } else {
            "Idle"
        }
    }

    pub fn reader_count(&self) -> usize {
        self.readers.len()
    }

    pub fn writer_count(&self) -> usize {
        self.writers.len()
    }

    pub fn waiting_count(&self) -> usize {
        self.waiting.len()
    }

    pub fn is_safe(&self) -> bool {
        (self.readers.is_empty() || self.writers.is_empty()) && self.writers.len() <= 1
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolAction {
    TryRead(i64),
    TryWrite(i64),
    ReadOrWrite,
    Stop(i64),
}

impl ProtocolAction {
    pub fn name(&self) -> &'static str {
        match self {
            Self::TryRead(_) => "TryRead",
            Self::TryWrite(_) => "TryWrite",
            Self::ReadOrWrite => "ReadOrWrite",
            Self::Stop(_) => "Stop",
        }
    }

    pub fn actor(&self) -> Option<i64> {
        match self {
            Self::TryRead(actor) | Self::TryWrite(actor) | Self::Stop(actor) => Some(*actor),
            Self::ReadOrWrite => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransitionKind {
    Enqueued,
    Unchanged,
    ReaderStartedFromIdle,
    ReaderStartedFromReading,
    WriterStarted,
    ReaderStoppedMoreRemain,
    ReaderStoppedLast,
    WriterStopped,
}

impl TransitionKind {
    pub fn callback_action(&self) -> &'static str {
        match self {
            Self::Enqueued => "Enqueued",
            Self::Unchanged => "Unchanged",
            Self::ReaderStartedFromIdle => "ReaderStartedFromIdle",
            Self::ReaderStartedFromReading => "ReaderStartedFromReading",
            Self::WriterStarted => "WriterStarted",
            Self::ReaderStoppedMoreRemain => "ReaderStoppedMoreRemain",
            Self::ReaderStoppedLast => "ReaderStoppedLast",
            Self::WriterStopped => "WriterStopped",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionOutcome {
    pub before: RwState,
    pub after: RwState,
    pub kind: TransitionKind,
}

pub fn apply_protocol_action(
    before: &RwState,
    action: &ProtocolAction,
) -> Result<TransitionOutcome, String> {
    let mut after = before.clone();
    let before_status = before.status();

    let kind = match action {
        ProtocolAction::TryRead(actor) => {
            if after
                .waiting
                .iter()
                .any(|req| req.kind == RequestKind::Read && req.actor == *actor)
            {
                TransitionKind::Unchanged
            } else {
                after.waiting.push(WaitRequest {
                    kind: RequestKind::Read,
                    actor: *actor,
                });
                TransitionKind::Enqueued
            }
        }
        ProtocolAction::TryWrite(actor) => {
            if after
                .waiting
                .iter()
                .any(|req| req.kind == RequestKind::Write && req.actor == *actor)
            {
                TransitionKind::Unchanged
            } else {
                after.waiting.push(WaitRequest {
                    kind: RequestKind::Write,
                    actor: *actor,
                });
                TransitionKind::Enqueued
            }
        }
        ProtocolAction::ReadOrWrite => {
            if before.waiting.is_empty() {
                return Err("ReadOrWrite disabled: waiting queue is empty".to_string());
            }
            if !before.writers.is_empty() {
                return Err("ReadOrWrite disabled: writer is active".to_string());
            }

            let head = after.waiting.remove(0);
            match head.kind {
                RequestKind::Read => {
                    after.readers.insert(head.actor);
                    if before_status == "Reading" {
                        TransitionKind::ReaderStartedFromReading
                    } else {
                        TransitionKind::ReaderStartedFromIdle
                    }
                }
                RequestKind::Write => {
                    if !before.readers.is_empty() {
                        return Err(
                            "ReadOrWrite disabled: writer is blocked by readers".to_string()
                        );
                    }
                    after.writers.insert(head.actor);
                    TransitionKind::WriterStarted
                }
            }
        }
        ProtocolAction::Stop(actor) => {
            if after.readers.remove(actor) {
                if after.readers.is_empty() {
                    TransitionKind::ReaderStoppedLast
                } else {
                    TransitionKind::ReaderStoppedMoreRemain
                }
            } else if after.writers.remove(actor) {
                TransitionKind::WriterStopped
            } else {
                return Err(format!("Stop disabled: actor {actor} is not active"));
            }
        }
    };

    if !after.is_safe() {
        return Err("transition would violate readers/writers safety".to_string());
    }

    Ok(TransitionOutcome {
        before: before.clone(),
        after,
        kind,
    })
}

pub fn parse_action(action_name: &str, params: &Value) -> Result<ProtocolAction, String> {
    match action_name {
        "TryRead" => Ok(ProtocolAction::TryRead(required_actor(params, "TryRead")?)),
        "TryWrite" => Ok(ProtocolAction::TryWrite(required_actor(
            params, "TryWrite",
        )?)),
        "ReadOrWrite" => Ok(ProtocolAction::ReadOrWrite),
        "Stop" => Ok(ProtocolAction::Stop(required_actor(params, "Stop")?)),
        other => Err(format!("unknown protocol action '{other}'")),
    }
}

pub fn state_from_fields(fields: &Value) -> RwState {
    RwState {
        readers: int_set(fields.get("readers")),
        writers: int_set(fields.get("writers")),
        waiting: wait_queue(fields.get("waiting")),
    }
}

pub fn proposal_params(outcome: &TransitionOutcome, action: &ProtocolAction) -> Value {
    json!({
        "proposed_readers": set_json(&outcome.after.readers),
        "proposed_writers": set_json(&outcome.after.writers),
        "proposed_waiting": waiting_json(&outcome.after.waiting),
        "last_step": action.name(),
        "actor": action.actor(),
        "error": Value::Null,
    })
}

pub fn callback_params(outcome: &TransitionOutcome, action: &ProtocolAction) -> Value {
    json!({
        "readers": set_json(&outcome.after.readers),
        "writers": set_json(&outcome.after.writers),
        "waiting": waiting_json(&outcome.after.waiting),
        "last_step": action.name(),
        "actor": action.actor(),
        "error": Value::Null,
    })
}

pub fn rejection_params(error: impl Into<String>, action_name: &str, actor: Option<i64>) -> Value {
    json!({
        "error": error.into(),
        "last_step": action_name,
        "actor": actor,
    })
}

pub fn validate_proposal(fields: &Value, params: &Value) -> Result<TransitionOutcome, String> {
    let action_name = params
        .get("last_step")
        .and_then(Value::as_str)
        .ok_or("ValidateProposal requires last_step")?;
    let action = parse_action(action_name, params)?;
    let before = state_from_fields(fields);
    let expected = apply_protocol_action(&before, &action)?;
    let proposed = RwState {
        readers: int_set(params.get("proposed_readers")),
        writers: int_set(params.get("proposed_writers")),
        waiting: wait_queue(params.get("proposed_waiting")),
    };

    if proposed != expected.after {
        return Err("proposal does not match the Quint transition relation".to_string());
    }
    if proposed.reader_count() != expected.after.reader_count()
        || proposed.writer_count() != expected.after.writer_count()
        || proposed.waiting_count() != expected.after.waiting_count()
    {
        return Err("proposal count mismatch".to_string());
    }
    if !proposed.is_safe() {
        return Err("proposal violates readers/writers safety".to_string());
    }

    Ok(expected)
}

fn required_actor(params: &Value, action_name: &str) -> Result<i64, String> {
    params
        .get("actor")
        .and_then(Value::as_i64)
        .ok_or_else(|| format!("{action_name} requires integer actor"))
}

fn int_set(value: Option<&Value>) -> BTreeSet<i64> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_i64)
        .collect()
}

fn wait_queue(value: Option<&Value>) -> Vec<WaitRequest> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let kind = match entry.get("kind").and_then(Value::as_str) {
                Some("read") => RequestKind::Read,
                Some("write") => RequestKind::Write,
                _ => return None,
            };
            let actor = entry.get("actor").and_then(Value::as_i64)?;
            Some(WaitRequest { kind, actor })
        })
        .collect()
}

fn set_json(values: &BTreeSet<i64>) -> Value {
    Value::Array(values.iter().copied().map(Value::from).collect())
}

fn waiting_json(waiting: &[WaitRequest]) -> Value {
    Value::Array(
        waiting
            .iter()
            .map(|req| json!({ "kind": req.kind.as_str(), "actor": req.actor }))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_read_request_is_unchanged() {
        let mut state = RwState::default();
        let first = apply_protocol_action(&state, &ProtocolAction::TryRead(1)).unwrap();
        state = first.after;
        let second = apply_protocol_action(&state, &ProtocolAction::TryRead(1)).unwrap();
        assert_eq!(second.kind, TransitionKind::Unchanged);
        assert_eq!(second.after.waiting.len(), 1);
    }

    #[test]
    fn writer_waits_behind_active_reader() {
        let state = RwState {
            readers: BTreeSet::from([1]),
            writers: BTreeSet::new(),
            waiting: vec![WaitRequest {
                kind: RequestKind::Write,
                actor: 2,
            }],
        };
        let err = apply_protocol_action(&state, &ProtocolAction::ReadOrWrite).unwrap_err();
        assert!(err.contains("blocked by readers"));
    }

    #[test]
    fn fifo_scenario_matches_quint_order() {
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

        let started = apply_protocol_action(&state, &ProtocolAction::ReadOrWrite).unwrap();
        assert_eq!(started.kind, TransitionKind::ReaderStartedFromIdle);
        assert_eq!(started.after.readers, BTreeSet::from([1]));
        assert_eq!(started.after.waiting[0].actor, 2);

        let blocked =
            apply_protocol_action(&started.after, &ProtocolAction::ReadOrWrite).unwrap_err();
        assert!(blocked.contains("blocked by readers"));

        state = apply_protocol_action(&started.after, &ProtocolAction::Stop(1))
            .unwrap()
            .after;
        let writer = apply_protocol_action(&state, &ProtocolAction::ReadOrWrite).unwrap();
        assert_eq!(writer.kind, TransitionKind::WriterStarted);
        assert_eq!(writer.after.writers, BTreeSet::from([2]));
    }
}
