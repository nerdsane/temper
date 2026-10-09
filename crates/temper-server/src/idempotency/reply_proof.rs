//! Commit-time authority for reconstructing a cold logical reply.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Trusted actor-call boundary. HTTP bodies/headers never deserialize this mode.
/// Dispatcher modes record the first obligations, not the retry's preferences.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ActionReplyMode {
    /// The caller observes only the actor's committed response.
    DirectCore,
    /// Atomic composite sub-write whose required post-append projection is not
    /// certified by the core commit. No final child reply is durably retained.
    Composite,
    /// Post-commit work can fail or replace the logical reply; no durable final
    /// completion proof is maintained for this mode.
    Dispatch {
        /// Whether the first request joins integration execution.
        await_integration: bool,
        /// Whether the first request waits for registered reactions.
        await_reactions: bool,
        /// Registered reaction scope, absent for core-only dispatch callers.
        reaction_depth: Option<u32>,
    },
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ReplyClass {
    Core,
    PostCompletion,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplyProof {
    version: u32,
    mode: ActionReplyMode,
    reply_class: ReplyClass,
    digest: String,
}

fn class(mode: ActionReplyMode) -> ReplyClass {
    match mode {
        ActionReplyMode::DirectCore => ReplyClass::Core,
        ActionReplyMode::Dispatch { .. } | ActionReplyMode::Composite => ReplyClass::PostCompletion,
    }
}

fn digest(key: &str, binding: &str, core: &str, mode: ActionReplyMode) -> String {
    let mut hash = Sha256::new();
    super::update_bytes(&mut hash, b"temper.idempotency.reply.v1");
    super::update_json(
        &mut hash,
        &serde_json::json!({
            "key":key,"binding":binding,"core":core,"mode":mode,"reply_class":class(mode)
        }),
        false,
    );
    super::hex_digest(hash)
}

pub(super) fn stamp(key: &str, binding: &str, core: &str, mode: ActionReplyMode) -> Value {
    serde_json::to_value(ReplyProof {
        version: 1,
        mode,
        reply_class: class(mode),
        digest: digest(key, binding, core, mode),
    })
    .expect("reply proof is JSON serializable")
}

pub(super) fn verifies_core(proof: Option<&Value>, key: &str, binding: &str, core: &str) -> bool {
    let Some(value) = proof else {
        return false;
    };
    let Ok(proof) = serde_json::from_value::<ReplyProof>(value.clone()) else {
        return false;
    };
    proof.version == 1
        && proof.mode == ActionReplyMode::DirectCore
        && proof.reply_class == ReplyClass::Core
        && proof.digest == digest(key, binding, core, proof.mode)
        && serde_json::to_value(&proof).ok().as_ref() == Some(value)
}
