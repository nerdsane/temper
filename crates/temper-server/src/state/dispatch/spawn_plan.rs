//! What a spawn request creates.
//!
//! The kernel creates a spawned child after its parent commits. The child is
//! created with [`SpawnPlan::initial_fields`] and then sent its initial
//! action. The parent's write check reads the same plan before the parent
//! commits, so the child it checks the parent's links against is the child
//! the kernel creates.

use std::sync::Arc;

use serde_json::Value;
use temper_jit::table::{RelatedMap, TransitionTable};
use temper_runtime::tenant::TenantId;

use crate::entity_actor::effects::SpawnRequest;
use crate::entity_actor::{EntityActor, EntityState};
use crate::state::ServerState;

/// How the kernel creates one spawned child.
pub(crate) struct SpawnPlan {
    /// The child's transition table.
    pub table: Arc<TransitionTable>,
    /// The fields the child is created with.
    pub initial_fields: Value,
    /// The action sent to the child once it is created, with its params.
    pub initializer: Option<(String, Value)>,
}

/// Why a spawn creates no child: the action it was refused for (`spawn` when
/// the refusal is not about the initial action) and the reason.
pub(crate) struct SpawnRefusal {
    /// The refused action.
    pub action: String,
    /// The reason.
    pub error: String,
}

impl SpawnRefusal {
    fn new(action: &str, error: impl Into<String>) -> Self {
        Self {
            action: action.to_string(),
            error: error.into(),
        }
    }
}

impl ServerState {
    /// The plan for creating the child `req` asks for, from the parent's
    /// action with `action_params`, or why the kernel creates no child.
    /// Defer the initializer check when an earlier pending spawn supplies its
    /// prestate, or when dispatch will check it with typed store errors. The
    /// preview interpreter or live actor then validates the actual child state.
    pub(crate) async fn spawn_plan(
        &self,
        tenant: &TenantId,
        parent_type: &str,
        parent_id: &str,
        req: &SpawnRequest,
        action_params: &Value,
        defer_initializer_check: bool,
    ) -> Result<SpawnPlan, SpawnRefusal> {
        let mut parent_fields = serde_json::Map::new();
        parent_fields.insert("parent_type".to_string(), Value::from(parent_type));
        parent_fields.insert("parent_id".to_string(), Value::from(parent_id));
        parent_fields.insert(
            format!("{}_id", to_snake_case(parent_type)),
            Value::from(parent_id),
        );
        let table = self
            .transition_table_for_dispatch(tenant, &req.entity_type)
            .map_err(|error| SpawnRefusal::new("spawn", error.to_string()))?;
        let strict_child = table.strict_action_params;
        if strict_child && req.initial_action.is_none() {
            return Err(SpawnRefusal::new(
                "spawn",
                "Strict child requires a declared initializer for its generated fields",
            ));
        }
        let initializer = match &req.initial_action {
            Some(action) => {
                // Copied parent fields fill in what this action's own params
                // do not carry; a param wins, so a value the spawning action
                // just set is never overwritten by the field as it stood
                // before the action.
                let mut params = req.copied_fields.clone();
                params.extend(action_params.as_object().cloned().unwrap_or_default());
                params.extend(parent_fields.clone());
                let params = self
                    .prepare_generated_action_params(
                        tenant,
                        &req.entity_type,
                        action,
                        Value::Object(params),
                    )
                    .map_err(|error| SpawnRefusal::new(action, error))?;
                Some((action.clone(), params))
            }
            None => None,
        };
        // A strict child's declared initializer accepts its data. The spawn
        // effect supplies only identity to generic creation.
        let initial_fields = if strict_child {
            serde_json::json!({})
        } else {
            Value::Object(parent_fields)
        };
        if let Some((action, params)) = &initializer
            && !defer_initializer_check
        {
            self.load_authz_resource_snapshot(tenant, &req.entity_type, &req.entity_id)
                .await
                .map_err(|error| error.to_string())
                .and_then(|snapshot| {
                    // Existing actors validate against their hydrated blob
                    // values and execution-time state. Only absent children
                    // need this check before creation can persist anything.
                    if snapshot.exists {
                        return Ok(());
                    }
                    let prestate = EntityActor::build_initial_state(
                        &req.entity_type,
                        &req.entity_id,
                        &table,
                        &initial_fields,
                    );
                    table
                        .validate_action_params(
                            action,
                            params,
                            &prestate.fields,
                            &prestate.counters,
                            &prestate.booleans,
                        )
                        .map_err(String::from)
                })
                .map_err(|error| SpawnRefusal::new(action, error))?;
        }
        Ok(SpawnPlan {
            table,
            initial_fields,
            initializer,
        })
    }

    /// The related records the guards of `action` on `entity_type` read,
    /// resolved from `fields`, for a record that is not stored yet. A related
    /// record that is `committed` (the spawning parent, which commits before
    /// its child's initial action runs) is read as that state.
    pub(crate) async fn resolve_guards_from_fields(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        action: &str,
        fields: &Value,
        committed: &EntityState,
    ) -> RelatedMap {
        let refs = self.guard_related_refs(tenant, entity_type, action);
        let Ok(table) = self.transition_table_for_dispatch(tenant, entity_type) else {
            return RelatedMap::new();
        };
        self.resolve_guard_refs(tenant, &table, refs, fields, Some(committed))
            .await
    }
}

fn to_snake_case(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    for (index, ch) in value.chars().enumerate() {
        match ch {
            'A'..='Z' => {
                if index > 0 {
                    result.push('_');
                }
                result.push(ch.to_ascii_lowercase());
            }
            '-' | ' ' => result.push('_'),
            _ => result.push(ch.to_ascii_lowercase()),
        }
    }
    result
}
