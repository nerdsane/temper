//! What a request asked for, so an `Idempotency-Key` only ever answers a
//! retry of the request it was first used for.

use sha2::{Digest, Sha256};

/// An `Idempotency-Key`, and what a retry under it must repeat to be answered
/// by it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdempotencyKey {
    /// Names one request: a retry must send the same action and parameters.
    /// A caller's key is one.
    Request(String),
    /// Names a follow-up by its cause (a callback, scheduled action or
    /// reaction), derived from the key of the request that started it. A
    /// retry of that request starts the follow-up again, and what it sends
    /// may differ: an outside service can answer otherwise, or fail where it
    /// succeeded, so a callback can even name the other action. The cause
    /// has one outcome, so a retry is answered by the first one.
    FollowUp(String),
}

impl IdempotencyKey {
    /// The key's text.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Request(key) | Self::FollowUp(key) => key,
        }
    }

    /// The key's text, owned.
    pub fn into_string(self) -> String {
        match self {
            Self::Request(key) | Self::FollowUp(key) => key,
        }
    }

    /// What a retry of `action` called with `params` under this key repeats.
    pub fn request(&self, action: &str, params: &serde_json::Value) -> RequestFingerprint {
        match self {
            Self::Request(_) => RequestFingerprint::new(action, params),
            Self::FollowUp(_) => RequestFingerprint::Cause {
                action: action.to_string(),
            },
        }
    }
}

/// What a key's retry must repeat to be answered by the key.
///
/// Object keys are hashed in sorted order, so a retry that sends the same body
/// with its keys in another order is the same request.
#[derive(Debug, Clone, Eq)]
pub enum RequestFingerprint {
    /// A caller's request: the same action and a digest of its parameters.
    Request {
        action: String,
        params_digest: [u8; 32],
    },
    /// A follow-up ([`IdempotencyKey::FollowUp`]): any retry of its cause,
    /// whatever action it names now. `action` is what it named.
    Cause { action: String },
}

impl PartialEq for RequestFingerprint {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Request {
                    action,
                    params_digest,
                },
                Self::Request {
                    action: other_action,
                    params_digest: other_digest,
                },
            ) => action == other_action && params_digest == other_digest,
            (Self::Cause { .. }, Self::Cause { .. }) => true,
            _ => false,
        }
    }
}

impl RequestFingerprint {
    /// Fingerprint a caller's `action` called with `params`.
    pub fn new(action: &str, params: &serde_json::Value) -> Self {
        let mut hasher = Sha256::new();
        hash_params(&mut hasher, params);
        Self::Request {
            action: action.to_string(),
            params_digest: hasher.finalize().into(),
        }
    }

    /// The action the request named.
    pub fn action(&self) -> &str {
        match self {
            Self::Request { action, .. } | Self::Cause { action } => action,
        }
    }

    /// Whether a retry must name the action the key recorded.
    pub fn repeats_action(&self) -> bool {
        matches!(self, Self::Request { .. })
    }
}

/// The idempotency key of a follow-up request (a reaction, callback or
/// scheduled action) that the request under `parent` starts.
///
/// `label` names the follow-up (`callback`, `scheduled:<action>`)
/// and `detail` tells siblings with the same label apart. A retry of the parent
/// derives the same key. The key is a hash, not the parent with more text
/// appended, so it stays the same length however long a chain of follow-ups
/// runs: a self-rescheduling action keeps a key of constant size every cycle.
pub fn derived_idempotency_key(parent: &str, label: &str, detail: &str) -> String {
    let mut hasher = Sha256::new();
    for part in [parent, label, detail] {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    let digest: String = hasher.finalize()[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("derived:{label}:{digest}")
}

enum Item<'a> {
    Value(&'a serde_json::Value),
    Key(&'a str),
}

/// Hash a JSON value with a tag and length before every part, so two
/// different values never feed the hasher the same bytes. Walked without
/// recursion.
fn hash_params(hasher: &mut Sha256, params: &serde_json::Value) {
    let text = |hasher: &mut Sha256, tag: u8, bytes: &[u8]| {
        hasher.update([tag]);
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    };
    let mut pending = vec![Item::Value(params)];
    while let Some(item) = pending.pop() {
        match item {
            Item::Key(name) => text(hasher, b'k', name.as_bytes()),
            Item::Value(serde_json::Value::Null) => hasher.update(b"z"),
            Item::Value(serde_json::Value::Bool(value)) => {
                hasher.update(if *value { b"t" } else { b"f" })
            }
            Item::Value(serde_json::Value::Number(number)) => {
                text(hasher, b'n', number.to_string().as_bytes())
            }
            Item::Value(serde_json::Value::String(value)) => text(hasher, b's', value.as_bytes()),
            Item::Value(serde_json::Value::Array(items)) => {
                hasher.update(b"a");
                hasher.update((items.len() as u64).to_le_bytes());
                pending.extend(items.iter().rev().map(Item::Value));
            }
            Item::Value(serde_json::Value::Object(map)) => {
                hasher.update(b"o");
                hasher.update((map.len() as u64).to_le_bytes());
                let mut entries: Vec<_> = map.iter().collect();
                entries.sort_by_key(|(name, _)| *name);
                for (name, value) in entries.into_iter().rev() {
                    pending.push(Item::Value(value));
                    pending.push(Item::Key(name));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RequestFingerprint;
    use serde_json::json;

    #[test]
    fn the_same_body_in_another_key_order_is_the_same_request() {
        let first = RequestFingerprint::new("Note", &json!({"a": 1, "b": {"c": [1, "x"]}}));
        let second = RequestFingerprint::new("Note", &json!({"b": {"c": [1, "x"]}, "a": 1}));
        assert_eq!(first, second);
    }

    #[test]
    fn another_action_or_body_is_another_request() {
        let note = RequestFingerprint::new("Note", &json!({"a": 1}));
        assert_ne!(note, RequestFingerprint::new("Close", &json!({"a": 1})));
        assert_ne!(note, RequestFingerprint::new("Note", &json!({"a": 2})));
        assert_ne!(note, RequestFingerprint::new("Note", &json!({"a": "1"})));
        assert_ne!(
            RequestFingerprint::new("Note", &json!(["ab", "c"])),
            RequestFingerprint::new("Note", &json!(["a", "bc"]))
        );
    }

    #[test]
    fn a_request_retry_repeats_its_body_and_a_follow_up_retry_is_its_cause_again() {
        use super::IdempotencyKey;

        let request = IdempotencyKey::Request("k".to_string());
        assert_ne!(
            request.request("Answered", &json!({"duration_ms": 17})),
            request.request("Answered", &json!({"duration_ms": 42}))
        );
        let follow_up = IdempotencyKey::FollowUp("k".to_string());
        assert_eq!(
            follow_up.request("Answered", &json!({"duration_ms": 17})),
            follow_up.request("Answered", &json!({"duration_ms": 42}))
        );
        assert_eq!(
            follow_up.request("Answered", &json!({})),
            follow_up.request("Unanswered", &json!({})),
            "an integration run again may now fail: its first outcome answers"
        );
        assert_ne!(
            request.request("Answered", &json!({})),
            follow_up.request("Answered", &json!({}))
        );
    }

    #[test]
    fn a_derived_key_keeps_its_length_down_a_chain_and_repeats_for_a_retry() {
        use super::derived_idempotency_key;

        let first = derived_idempotency_key("caller-key", "scheduled:Tick", "0");
        assert_eq!(
            first,
            derived_idempotency_key("caller-key", "scheduled:Tick", "0")
        );
        assert_ne!(
            first,
            derived_idempotency_key("caller-key", "scheduled:Tick", "1")
        );
        assert_ne!(
            first,
            derived_idempotency_key("other-key", "scheduled:Tick", "0")
        );

        let mut key = first.clone();
        for _ in 0..1_000 {
            let next = derived_idempotency_key(&key, "scheduled:Tick", "0");
            assert_ne!(next, key);
            assert_eq!(next.len(), first.len());
            key = next;
        }
    }
}
