use std::collections::BTreeMap;
use temper_jit::table::EvalContext;

/// Serializable state for spec-driven actors.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct SpecActorState {
    pub status: String,
    #[serde(default)]
    pub counters: BTreeMap<String, usize>,
    #[serde(default)]
    pub booleans: BTreeMap<String, bool>,
    #[serde(default)]
    pub lists: BTreeMap<String, Vec<String>>,
    /// Arbitrary extra data — used to thread params through the reaction chain.
    /// SpecDrivenActor stores the last incoming message params here so integrations
    /// can read them from the trigger message.
    #[serde(default)]
    pub fields: serde_json::Value,
}

impl SpecActorState {
    pub(super) fn to_eval_context(&self) -> EvalContext {
        let mut ctx = EvalContext::default();
        for (k, v) in &self.counters {
            ctx.counters.insert(k.clone(), *v);
        }
        for (k, v) in &self.booleans {
            ctx.booleans.insert(k.clone(), *v);
        }
        for (k, v) in &self.lists {
            ctx.lists.insert(k.clone(), v.clone());
        }
        ctx
    }
}
