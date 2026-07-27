//! Durable actor idempotency receipts and deterministic request binding.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::effects::{ScheduledAction, SpawnRequest};

/// Durable outcome associated with an idempotency key.
///
/// The legacy numeric form keeps existing snapshots readable. New entries bind
/// the key to the exact action, parameters, and ordered post-commit effects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DurableIdempotencyOutcome {
    /// Sequence-only entry written by earlier Temper versions.
    LegacySequence(u64),
    /// Complete outcome written for new transitions.
    Completed {
        /// Durable event sequence.
        sequence_nr: u64,
        /// Exact action bound to this key.
        action: String,
        /// Stable digest of the exact committed action parameters.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        params_digest: Option<[u8; 32]>,
        /// Exact transition definition that produced the persisted outputs.
        /// `None` is reserved for legacy events without a receipt.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effect_receipt_version: Option<[u8; 32]>,
        /// Ordered custom effects needing idempotent post-commit dispatch.
        #[serde(default)]
        custom_effects: Vec<String>,
        /// Ordered scheduled actions produced by the committed transition.
        #[serde(default)]
        scheduled_actions: Vec<ScheduledAction>,
        /// Ordered child spawns produced by the committed transition.
        #[serde(default)]
        spawn_requests: Vec<SpawnRequest>,
    },
}

impl DurableIdempotencyOutcome {
    pub(super) fn sequence_nr(&self) -> u64 {
        match self {
            Self::LegacySequence(sequence_nr) | Self::Completed { sequence_nr, .. } => *sequence_nr,
        }
    }
}

pub(super) fn idempotency_params_digest(params: &serde_json::Value) -> [u8; 32] {
    fn hash_value(hasher: &mut Sha256, value: &serde_json::Value) {
        match value {
            serde_json::Value::Null => hasher.update(b"n"),
            serde_json::Value::Bool(value) => hasher.update(if *value { b"b1" } else { b"b0" }),
            serde_json::Value::Number(value) => {
                let rendered = value.to_string();
                hasher.update(b"d");
                hasher.update((rendered.len() as u64).to_be_bytes());
                hasher.update(rendered.as_bytes());
            }
            serde_json::Value::String(value) => {
                hasher.update(b"s");
                hasher.update((value.len() as u64).to_be_bytes());
                hasher.update(value.as_bytes());
            }
            serde_json::Value::Array(values) => {
                hasher.update(b"a");
                hasher.update((values.len() as u64).to_be_bytes());
                for value in values {
                    hash_value(hasher, value);
                }
            }
            serde_json::Value::Object(values) => {
                hasher.update(b"o");
                hasher.update((values.len() as u64).to_be_bytes());
                let mut keys = values.keys().collect::<Vec<_>>();
                keys.sort_unstable();
                for key in keys {
                    hasher.update((key.len() as u64).to_be_bytes());
                    hasher.update(key.as_bytes());
                    hash_value(hasher, &values[key]);
                }
            }
        }
    }

    let mut hasher = Sha256::new();
    hash_value(&mut hasher, params);
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::idempotency_params_digest;

    #[test]
    fn parameter_digest_is_object_order_independent_and_value_sensitive() {
        let first: serde_json::Value = serde_json::from_str(r#"{"a":1,"b":[true,"x"]}"#).unwrap();
        let reordered: serde_json::Value =
            serde_json::from_str(r#"{"b":[true,"x"],"a":1}"#).unwrap();
        let changed: serde_json::Value =
            serde_json::from_str(r#"{"b":[false,"x"],"a":1}"#).unwrap();

        assert_eq!(
            idempotency_params_digest(&first),
            idempotency_params_digest(&reordered)
        );
        assert_ne!(
            idempotency_params_digest(&first),
            idempotency_params_digest(&changed)
        );
    }
}
