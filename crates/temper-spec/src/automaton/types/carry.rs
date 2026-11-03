//! `[[carry]]`: where a new version of a spec moves the stored records the
//! version it replaces would leave with no way forward.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::Automaton;
use crate::predicate::Expr;

/// The prefix of the name of every move that applies a `[[carry]]`.
pub const CARRY_PREFIX: &str = "carry:";

/// Where this version moves the stored records in `from` that it would
/// otherwise leave with no way forward: `from` is a state it drops, or one
/// where none of its moves is possible for them. Once an install of the
/// version has answered, and at each start while the version holds it, the
/// platform applies the carry to each such record as one move journaled like
/// any other. Records with a way forward are not moved.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Carry {
    /// The state the records are in. This version may no longer have it.
    pub from: String,
    /// Which of those records; all of them when absent.
    #[serde(default = "Expr::always", skip_serializing_if = "Expr::is_always")]
    pub guard: Expr,
    /// The state they go to; `from` when absent.
    pub to: Option<String>,
    /// Values the move writes: a counter, a flag, or a string or number
    /// field, each a state variable this version declares.
    #[serde(default)]
    pub set: BTreeMap<String, serde_json::Value>,
    /// An action of this version the platform makes on each carried record
    /// right after its carry, with no parameters, as an ordinary move with
    /// its effects, triggers and reactions, decided by the version's Cedar.
    /// A record whose next step is what that action does gets there. It must
    /// start from the carry's target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub then: Option<String>,
    /// The string field naming the user the `then` move is made for: the
    /// platform's carry service acts for them, so the version's Cedar sees
    /// them as `context.actingFor`. A record whose field is empty is left
    /// where the carry put it.
    #[serde(default, rename = "as", skip_serializing_if = "Option::is_none")]
    pub acts_for: Option<String>,
}

impl Carry {
    /// The state a carried record ends in.
    pub fn target(&self) -> &str {
        self.to.as_deref().unwrap_or(&self.from)
    }
}

/// Whether `action` names the move of a `[[carry]]`, which only the platform
/// makes.
pub fn is_carry_move(action: &str) -> bool {
    action.starts_with(CARRY_PREFIX)
}

impl Automaton {
    /// Each `[[carry]]` with the name of the move that applies it,
    /// `carry:<from>:<n>` for the n-th carry from that state, in spec order.
    pub fn carry_moves(&self) -> Vec<(String, &Carry)> {
        let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
        self.carries
            .iter()
            .map(|carry| {
                let n = seen.entry(carry.from.as_str()).or_default();
                *n += 1;
                (format!("{CARRY_PREFIX}{}:{n}", carry.from), carry)
            })
            .collect()
    }
}
