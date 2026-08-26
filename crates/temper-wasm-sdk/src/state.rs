//! Typed decoding at the IOA/WASM state boundary.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Number, Value};

#[derive(Debug, Deserialize)]
struct MemberStateEnvelope {
    status: String,
    fields: Map<String, Value>,
    counters: BTreeMap<String, u64>,
    booleans: BTreeMap<String, bool>,
    lists: BTreeMap<String, Vec<String>>,
}

/// Failure while crossing a typed IOA/WASM state boundary.
#[derive(Debug)]
#[non_exhaustive]
pub enum StateDecodeError {
    /// The host value was not the exact runtime member-state envelope.
    MemberEnvelope(serde_json::Error),
    /// The flattened member state did not match the caller's typed model.
    MemberState(serde_json::Error),
    /// Canonical migration source state did not match the caller's typed model.
    SourceState(serde_json::Error),
}

impl core::fmt::Display for StateDecodeError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::MemberEnvelope(error) => {
                write!(formatter, "invalid runtime member-state envelope: {error}")
            }
            Self::MemberState(error) => write!(formatter, "invalid typed member state: {error}"),
            Self::SourceState(error) => {
                write!(formatter, "invalid typed migration source state: {error}")
            }
        }
    }
}

impl std::error::Error for StateDecodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::MemberEnvelope(error) | Self::MemberState(error) | Self::SourceState(error) => {
                Some(error)
            }
        }
    }
}

/// Deserialize an exact runtime entity-state envelope into a typed IOA member state.
///
/// Ordinary fields establish the base object. Authoritative counters, booleans,
/// lists, and lifecycle status then overwrite their projected field copies. The
/// decoder does not accept a flat legacy object or probe alternate field names.
///
/// ```
/// # use serde::Deserialize;
/// # use serde_json::json;
/// # use temper_wasm_sdk::decode_member_state;
/// #[derive(Deserialize)]
/// struct TaskState {
///     status: String,
///     task_id: String,
///     attempts: u64,
/// }
///
/// let state: TaskState = decode_member_state(&json!({
///     "status": "Running",
///     "fields": {"task_id": "task-1"},
///     "counters": {"attempts": 2},
///     "booleans": {},
///     "lists": {}
/// }))?;
/// # assert_eq!(state.status, "Running");
/// # assert_eq!(state.task_id, "task-1");
/// # assert_eq!(state.attempts, 2);
/// # Ok::<(), temper_wasm_sdk::StateDecodeError>(())
/// ```
pub fn decode_member_state<T>(entity_state: &Value) -> Result<T, StateDecodeError>
where
    T: DeserializeOwned,
{
    let envelope: MemberStateEnvelope =
        serde_json::from_value(entity_state.clone()).map_err(StateDecodeError::MemberEnvelope)?;
    let mut member = envelope.fields;
    for (name, value) in envelope.counters {
        member.insert(name, Value::Number(Number::from(value)));
    }
    for (name, value) in envelope.booleans {
        member.insert(name, Value::Bool(value));
    }
    for (name, values) in envelope.lists {
        member.insert(
            name,
            Value::Array(values.into_iter().map(Value::String).collect()),
        );
    }
    member.insert("status".into(), Value::String(envelope.status));
    serde_json::from_value(Value::Object(member)).map_err(StateDecodeError::MemberState)
}

/// Deserialize canonical migration source-state JSON into a typed IOA state.
///
/// Source state is already a flat canonical object. This decoder performs no
/// envelope handling, name conversion, or alternate-name probing.
pub fn decode_source_state<T>(canonical_state_json: &str) -> Result<T, StateDecodeError>
where
    T: DeserializeOwned,
{
    let value: Map<String, Value> =
        serde_json::from_str(canonical_state_json).map_err(StateDecodeError::SourceState)?;
    serde_json::from_value(Value::Object(value)).map_err(StateDecodeError::SourceState)
}
