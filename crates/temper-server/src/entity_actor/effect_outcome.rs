//! Stored results of transition effects for deterministic replay.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// What a transition's effects wrote into the entity: every counter, boolean,
/// list and field whose value they changed, with its value afterwards.
///
/// Some effects produce their value only when they run (a spawn mints its
/// child's id), and every effect computes with the spec in force at that
/// moment. So the event records the result and replay restores it: replay
/// never mints, spawns, schedules or computes again. The record is the
/// difference between the state before and after the effects, so no effect
/// can write something the record leaves out. Effects only set values; none
/// removes a variable or a field.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EffectOutcome {
    /// Spawn fields whose reservation owner changed, even if the child id did
    /// not. Replay restores these identities instead of minting new ones.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub spawn_reservations: BTreeMap<String, String>,
    /// Counters the effects changed, with their new values.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub counters: BTreeMap<String, usize>,
    /// Booleans the effects changed, with their new values.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub booleans: BTreeMap<String, bool>,
    /// Lists the effects changed, with their new contents.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub lists: BTreeMap<String, Vec<String>>,
    /// Fields the effects wrote directly (a spawn's stored child id), with
    /// their new values. Action params reach fields from the event's params.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub fields: serde_json::Map<String, serde_json::Value>,
}
