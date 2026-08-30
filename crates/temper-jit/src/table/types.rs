//! Core types for transition tables.
//!
//! A [`TransitionTable`] encodes the complete set of transition rules for a single
//! entity type as DATA, not code. It can be hot-swapped per-actor without restart.
//!
//! Guard conditions and their evaluation live in [`super::guard`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::guard::{Guard, GuardFailure};

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

/// A declared unique/alternate key carried on the table (ADR-0153). `name`
/// identifies it; `properties` is the unique property set in canonical order.
/// The actor hashes these on write to maintain the negative-existence access path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeclaredKey {
    pub name: String,
    pub properties: Vec<String>,
}

/// A declared vector access path carried on the table (ADR-0155). `name`
/// identifies it; `property` is the float-vector state variable, `model_property`
/// the model-tag state variable that partitions the space, `dims` the expected
/// length, `metric` one of `cosine`/`dot`/`l2`. The actor parses and co-commits
/// these on write to maintain `entity_vector_index`; `Temper.Nearest` reads them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeclaredVector {
    pub name: String,
    pub property: String,
    pub model_property: String,
    pub dims: usize,
    pub metric: String,
}

/// A transition table: state machine transitions as DATA, not code.
/// Can be hot-swapped per-actor without restart.
#[derive(Debug, Clone, Serialize)]
pub struct TransitionTable {
    /// The entity this table governs (e.g. "Order").
    pub entity_name: String,
    /// All valid state values.
    pub states: Vec<String>,
    /// The state an entity starts in.
    pub initial_state: String,
    /// Ordered list of transition rules.
    pub rules: Vec<TransitionRule>,
    /// ADR-0153: declared unique/alternate keys the kernel indexes for
    /// negative-existence reads. Empty when the spec declared no `[[key]]`.
    pub keys: Vec<DeclaredKey>,
    /// ADR-0155: declared vector access paths the kernel indexes for exact-scan
    /// kNN reads (`Temper.Nearest`). Empty when the spec declared no `[[vector]]`.
    #[serde(default)]
    pub vectors: Vec<DeclaredVector>,
    /// Per-state-variable metadata for platform primitives (ADR-0045, ADR-0047).
    /// Keyed by state-variable name. Empty map when the IOA spec did not
    /// declare any per-field overrides.
    #[serde(default)]
    pub state_var_metadata: BTreeMap<String, StateVarMetadata>,
    /// Composite-action metadata keyed by action name (ADR-0040).
    #[serde(default)]
    pub composite_actions: BTreeMap<String, CompositeActionMetadata>,
    /// Declared input contract keyed by action name. Older serialized tables
    /// default to an empty map; newly compiled IOA parameters are required
    /// unless their metadata explicitly permits null.
    #[serde(default)]
    pub action_params: BTreeMap<String, Vec<ActionParamMetadata>>,
    /// Pre-built index: action name → indices into `rules`.
    ///
    /// Eliminates the O(N) linear scan + Vec allocation in [`evaluate_ctx()`].
    /// Rebuilt automatically during construction and deserialization.
    ///
    /// [`evaluate_ctx()`]: TransitionTable::evaluate_ctx
    #[serde(skip)]
    pub(crate) rule_index: BTreeMap<String, Vec<usize>>,
}

/// Compiled schema metadata for one action input.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActionParamMetadata {
    /// Logical parameter name as declared by the IOA action.
    pub name: String,
    /// IOA/CSDL type name used for adapter validation and diagnostics.
    pub param_type: String,
    /// Whether omission or JSON `null` is accepted.
    #[serde(default)]
    pub nullable: bool,
}

/// Platform-primitive metadata for a single state variable.
///
/// - `overflow_inline_max_bytes` (ADR-0045): serialized-byte ceiling above
///   which the value is moved to the content-addressed blob store. `None`
///   falls back to the crate-wide `DEFAULT_FIELD_INLINE_MAX` (128KB).
/// - `overflow_ttl_seconds` (ADR-0047): TTL for overflow blobs written on
///   behalf of this field. `None` = permanent (pre-ADR behavior).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct StateVarMetadata {
    /// Per-field inline byte ceiling for field overflow (ADR-0045).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overflow_inline_max_bytes: Option<usize>,
    /// Per-field TTL in seconds for overflow blobs (ADR-0047).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overflow_ttl_seconds: Option<u64>,
}

/// Parsed metadata for a first-class Composite action.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompositeActionMetadata {
    /// Cedar gate evaluated for the composite intent, if declared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cedar_gate: Option<CompositeCedarGate>,
    /// Whether the composite records an audit/idempotency event on the parent stream.
    #[serde(default = "default_record_parent_event")]
    pub record_parent_event: bool,
    /// Declared sub-write contract for the composite action.
    #[serde(default)]
    pub sub_writes: Vec<SubWriteSpec>,
}

impl Default for CompositeActionMetadata {
    fn default() -> Self {
        Self {
            cedar_gate: None,
            record_parent_event: true,
            sub_writes: Vec::new(),
        }
    }
}

fn default_record_parent_event() -> bool {
    true
}

/// The single Cedar gate evaluated for a Composite action.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompositeCedarGate {
    /// Principal expression, usually `request.principal`.
    pub principal: String,
    /// Resource expression, usually `this`.
    pub resource: String,
    /// Cedar action identifier for the composite intent.
    pub action: String,
}

/// Declared write shape emitted by a Composite action.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubWriteSpec {
    /// Target entity type receiving the write.
    pub target_entity: String,
    /// Target action, e.g. `Create` or `Update`.
    pub action: String,
    /// Handler input or source that generates this write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated_from: Option<String>,
}

impl<'de> Deserialize<'de> for TransitionTable {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        /// Helper struct for deserializing the persistent fields only.
        #[derive(Deserialize)]
        struct TransitionTableRaw {
            entity_name: String,
            states: Vec<String>,
            initial_state: String,
            rules: Vec<TransitionRule>,
            #[serde(default)]
            state_var_metadata: BTreeMap<String, StateVarMetadata>,
            #[serde(default)]
            composite_actions: BTreeMap<String, CompositeActionMetadata>,
            #[serde(default)]
            action_params: BTreeMap<String, Vec<ActionParamMetadata>>,
            #[serde(default)]
            keys: Vec<DeclaredKey>,
            #[serde(default)]
            vectors: Vec<DeclaredVector>,
        }

        let raw = TransitionTableRaw::deserialize(deserializer)?;
        let mut table = TransitionTable {
            entity_name: raw.entity_name,
            states: raw.states,
            initial_state: raw.initial_state,
            rules: raw.rules,
            keys: raw.keys,
            vectors: raw.vectors,
            state_var_metadata: raw.state_var_metadata,
            composite_actions: raw.composite_actions,
            action_params: raw.action_params,
            rule_index: BTreeMap::new(),
        };
        table.rebuild_index();
        Ok(table)
    }
}

/// A single transition rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransitionRule {
    /// Action name (e.g. "SubmitOrder").
    pub name: String,
    /// States this transition may fire from.
    pub from_states: Vec<String>,
    /// Target state after the transition (if deterministic).
    pub to_state: Option<String>,
    /// Guard condition evaluated before the transition fires.
    pub guard: Guard,
    /// Effects applied after the transition fires.
    pub effects: Vec<Effect>,
}

/// An effect applied after a transition fires.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Effect {
    /// Change the entity status.
    SetState(String),
    /// Add an item (legacy alias for `IncrementCounter("items")`).
    IncrementItems,
    /// Remove an item (legacy alias for `DecrementCounter("items")`).
    DecrementItems,
    /// Increment a named counter variable.
    IncrementCounter(String),
    /// Increment a named counter variable by a numeric action parameter.
    IncrementCounterByParam { var: String, param: String },
    /// Decrement a named counter variable.
    DecrementCounter(String),
    /// Decrement a named counter variable by a numeric action parameter.
    DecrementCounterByParam { var: String, param: String },
    /// Set a named counter variable from an action param.
    SetCounterFromParam { var: String, param: String },
    /// Set a named boolean variable.
    SetBool { var: String, value: bool },
    /// Emit a named event.
    EmitEvent(String),
    /// Append a value to a named list variable (value from action params).
    ListAppend(String),
    /// Remove a value from a named list variable by index (index from action params).
    ListRemoveAt(String),
    /// Domain-specific custom effect (e.g., "DeploySpecs", "NotifyAdmin").
    ///
    /// Dispatched by post-transition hooks registered at startup.
    /// The actor runtime ignores unknown custom effects — they are only
    /// meaningful to the hook that registered for them.
    Custom(String),
    /// Schedule a delayed action (timer fires after delay_seconds).
    ScheduleAction { action: String, delay_seconds: u64 },
    /// Schedule an action at an absolute timestamp read from an entity field.
    ScheduleAtAction { action: String, field: String },
    /// Spawn a child entity as a post-transition effect.
    SpawnEntity {
        entity_type: String,
        entity_id_source: String,
        initial_action: Option<String>,
        store_id_in: Option<String>,
        copy_fields: Option<Vec<String>>,
    },
}

/// The result of evaluating a transition.
#[derive(Debug, Clone, PartialEq)]
pub struct TransitionResult {
    /// The new state after the transition (may be unchanged).
    pub new_state: String,
    /// Effects that were applied.
    pub effects: Vec<Effect>,
    /// Whether the transition succeeded.
    pub success: bool,
    /// Identity of the first failing sub-guard, populated only on guard
    /// rejection (ADR-0151). `None` on success and on a from-state miss.
    pub guard_failure: Option<GuardFailure>,
}

impl TransitionTable {
    /// Add canonical IOA parameter keys for any accepted naming aliases.
    ///
    /// Existing keys are preserved because some internal create paths carry
    /// both CSDL and storage aliases. Semantic consumers can always read the
    /// exact IOA name after this normalization step.
    pub fn canonicalize_action_params(
        &self,
        action: &str,
        params: &serde_json::Value,
    ) -> serde_json::Value {
        let Some(metadata) = self.action_params.get(action) else {
            return params.clone();
        };
        let Some(source) = params.as_object() else {
            return params.clone();
        };
        let mut canonical = source.clone();
        for param in metadata {
            if canonical.contains_key(&param.name) {
                continue;
            }
            let normalized_name = temper_spec::naming::to_snake_case(&param.name);
            if let Some((_, value)) = source
                .iter()
                .find(|(name, _)| temper_spec::naming::to_snake_case(name) == normalized_name)
            {
                canonical.insert(param.name.clone(), value.clone());
            }
        }
        serde_json::Value::Object(canonical)
    }

    /// Validate required action inputs before guards or effects run.
    ///
    /// This actor-local defense checks presence and nullability. Adapters use
    /// pinned CSDL for the richer client-facing type validation.
    pub fn validate_required_action_params(
        &self,
        action: &str,
        params: &serde_json::Value,
    ) -> Result<(), ActionInputError> {
        let Some(metadata) = self.action_params.get(action) else {
            // Older serialized tables deserialize with an empty map. They stay
            // readable, but a known live action without compiled requiredness
            // metadata must not skip actor-local validation.
            if self.rule_index.contains_key(action) {
                return Err(ActionInputError {
                    code: "MissingActionParameter",
                    action: action.to_string(),
                    parameter: action.to_string(),
                });
            }
            return Ok(());
        };
        let object = params.as_object();
        for param in metadata {
            if param.nullable {
                continue;
            }
            let normalized_name = temper_spec::naming::to_snake_case(&param.name);
            let has_value = object.is_some_and(|object| {
                object.iter().any(|(name, value)| {
                    temper_spec::naming::to_snake_case(name) == normalized_name && !value.is_null()
                })
            });
            if !has_value {
                return Err(ActionInputError {
                    code: "MissingActionParameter",
                    action: action.to_string(),
                    parameter: param.name.clone(),
                });
            }
        }
        Ok(())
    }

    /// Rebuild the rule index from the current rules vec.
    ///
    /// Called automatically during construction. Must be called explicitly
    /// after deserialization (since `rule_index` is `#[serde(skip)]`).
    pub fn rebuild_index(&mut self) {
        self.rule_index.clear();
        for (i, rule) in self.rules.iter().enumerate() {
            self.rule_index
                .entry(rule.name.clone())
                .or_default()
                .push(i);
        }
    }
}

/// Stable actor-local action input validation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionInputError {
    /// Stable detail code shared with adapter diagnostics.
    pub code: &'static str,
    /// Action whose input contract was violated.
    pub action: String,
    /// Missing or null required parameter.
    pub parameter: String,
}

impl std::fmt::Display for ActionInputError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{}: action '{}' requires non-null parameter '{}'",
            self.code, self.action, self.parameter
        )
    }
}

impl std::error::Error for ActionInputError {}

#[cfg(test)]
mod tests {
    use super::super::guard::Guard;
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn rebuild_index_groups_by_name() {
        let mut table = TransitionTable {
            entity_name: "TestEntity".to_string(),
            states: vec!["Draft".to_string(), "Active".to_string()],
            initial_state: "Draft".to_string(),
            keys: vec![],
            vectors: vec![],
            rules: vec![
                TransitionRule {
                    name: "Submit".to_string(),
                    from_states: vec!["Draft".to_string()],
                    to_state: Some("Active".to_string()),
                    guard: Guard::Always,
                    effects: vec![],
                },
                TransitionRule {
                    name: "Submit".to_string(),
                    from_states: vec!["Active".to_string()],
                    to_state: Some("Draft".to_string()),
                    guard: Guard::Always,
                    effects: vec![],
                },
                TransitionRule {
                    name: "Cancel".to_string(),
                    from_states: vec!["Draft".to_string()],
                    to_state: Some("Draft".to_string()),
                    guard: Guard::Always,
                    effects: vec![],
                },
            ],
            state_var_metadata: BTreeMap::new(),
            composite_actions: BTreeMap::new(),
            action_params: BTreeMap::new(),
            rule_index: BTreeMap::new(),
        };
        table.rebuild_index();

        assert_eq!(table.rule_index.len(), 2);
        assert_eq!(table.rule_index["Submit"], vec![0, 1]);
        assert_eq!(table.rule_index["Cancel"], vec![2]);
    }

    #[test]
    fn compiled_action_contract_rejects_missing_and_null() {
        let table = TransitionTable::from_ioa_source(
            r#"
[automaton]
name = "Task"
states = ["Open"]
initial = "Open"

[[action]]
name = "Assign"
kind = "input"
from = ["Open"]
params = [
  { name = "agent_id", type = "Edm.String" },
  { name = "note", type = "Edm.String", nullable = true },
]
"#,
        );

        for input in [serde_json::json!({}), serde_json::json!({"AgentId": null})] {
            let error = table
                .validate_required_action_params("Assign", &input)
                .unwrap_err();
            assert_eq!(error.code, "MissingActionParameter");
        }
        for input in [
            serde_json::json!({"AgentId": "a"}),
            serde_json::json!({"AgentId": 3}),
            serde_json::json!({"AgentId": ""}),
            serde_json::json!({"AgentId": "a", "agent_id": "a"}),
            serde_json::json!({"agent_id": "a", "note": null}),
            serde_json::json!({"agent_id": "a", "note": "ok"}),
        ] {
            table
                .validate_required_action_params("Assign", &input)
                .unwrap();
        }
    }

    #[test]
    fn action_parameter_aliases_gain_the_exact_ioa_key() {
        let table = TransitionTable::from_ioa_source(
            r#"
[automaton]
name = "Counter"
states = ["Ready"]
initial = "Ready"

[[action]]
name = "Adjust"
from = ["Ready"]
params = [{ name = "delta_value", type = "Edm.Int64" }]
"#,
        );
        let normalized =
            table.canonicalize_action_params("Adjust", &serde_json::json!({"DeltaValue": 4}));
        assert_eq!(normalized["delta_value"], 4);
        assert_eq!(normalized["DeltaValue"], 4);
    }

    #[test]
    fn older_serialized_table_without_action_metadata_remains_readable() {
        let json = r#"{
          "entity_name":"Order",
          "states":["Draft","Cancelled"],
          "initial_state":"Draft",
          "rules":[{
            "name":"CancelOrder",
            "from_states":["Draft"],
            "to_state":"Cancelled",
            "guard":"Always",
            "effects":[{"SetState":"Cancelled"}]
          }],
          "keys":[],
          "vectors":[],
          "state_var_metadata":{},
          "composite_actions":{}
        }"#;
        let table: TransitionTable = serde_json::from_str(json).expect("old table fixture");
        assert!(table.action_params.is_empty());
        let error = table
            .validate_required_action_params("CancelOrder", &serde_json::json!({}))
            .unwrap_err();
        assert_eq!(error.code, "MissingActionParameter");
    }
}
