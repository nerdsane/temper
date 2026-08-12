//! Declared action-parameter boundary.
//!
//! The filtering logic itself lives in [`temper_jit::params`] so that both the
//! legacy in-process runtime (this crate) and the pg-actor runtime
//! (`temper-actor-runtime`) enforce the boundary through the *same* code. These
//! re-exports keep the existing call sites (`effects::process_action`,
//! `odata::action_params`) stable, and the tests below assert the boundary end
//! to end through [`process_action`](crate::entity_actor::effects::process_action).

pub(crate) use temper_jit::params::{
    ParamContractError, restrict_to_declared_params, undeclared_param_keys,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity_actor::effects::process_action;
    use crate::entity_actor::types::EntityState;
    use std::collections::BTreeSet;
    use temper_jit::TransitionTable;

    fn work_summary_table() -> TransitionTable {
        TransitionTable::from_ioa_source(
            r#"
[automaton]
name = "WorkSummary"
states = ["Open", "Done"]
initial = "Open"

[[action]]
name = "AttachVector"
kind = "input"
from = ["Done"]
to = "Done"
params = ["semantic_vector", "semantic_vector_model"]
"#,
        )
    }

    fn state(entity_type: &str) -> EntityState {
        EntityState {
            entity_type: entity_type.into(),
            entity_id: "arn-247".into(),
            status: "Done".into(),
            item_count: 0,
            counters: Default::default(),
            booleans: Default::default(),
            lists: Default::default(),
            fields: serde_json::json!({}),
            events: Default::default(),
            total_event_count: 0,
            events_since_snapshot: 0,
            last_snapshot_sequence_nr: 0,
            sequence_nr: 0,
            processed_idempotency_keys: Default::default(),
        }
    }

    #[test]
    fn undeclared_body_keys_do_not_mutate_fields() {
        let _guard = temper_runtime::scheduler::install_deterministic_context(247);
        let table = work_summary_table();
        let mut state = state("WorkSummary");
        state.fields = serde_json::json!({
            "goal": "original goal",
            "outcome": "shipped honestly",
        });
        let result = process_action(
            &mut state,
            &table,
            "AttachVector",
            &serde_json::json!({
                "semantic_vector": "[0.1,0.2]",
                "semantic_vector_model": "text-embedding-3-small",
                "goal": "HIJACKED",
                "outcome": "REWRITTEN",
            }),
        );
        assert!(result.success);
        assert_eq!(state.fields["semantic_vector"], "[0.1,0.2]");
        assert_eq!(
            state.fields["semantic_vector_model"],
            "text-embedding-3-small"
        );
        assert_eq!(state.fields["goal"], "original goal");
        assert_eq!(state.fields["outcome"], "shipped honestly");
    }

    #[test]
    fn known_action_without_metadata_fails_closed() {
        let mut table = work_summary_table();
        table.action_params.clear();
        let params = serde_json::json!({ "goal": "smuggled" });
        assert!(undeclared_param_keys(&table, "AttachVector", &params).is_err());

        let mut entity_state = state("WorkSummary");
        let result = process_action(&mut entity_state, &table, "AttachVector", &params);
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("missing declared-parameter metadata"))
        );
        assert!(entity_state.fields.get("goal").is_none());
    }

    #[test]
    fn exact_and_pascal_alias_together_are_rejected_before_mutation() {
        let table = TransitionTable::from_ioa_source(
            r#"
[automaton]
name = "Account"
states = ["Done"]
initial = "Done"

[[action]]
name = "UpdateOwner"
kind = "input"
from = ["Done"]
to = "Done"
params = ["user_id"]
"#,
        );
        let params = serde_json::json!({
            "user_id": "attacker",
            "UserId": "victim",
        });
        assert!(matches!(
            undeclared_param_keys(&table, "UpdateOwner", &params),
            Err(ParamContractError::AmbiguousAlias { .. })
        ));

        let mut state = state("Account");
        let result = process_action(&mut state, &table, "UpdateOwner", &params);
        assert!(!result.success);
        assert!(result.error.as_deref().is_some_and(|error| {
            error.contains("both 'user_id'") && error.contains("'UserId'")
        }));
        assert!(state.fields.get("user_id").is_none());
        assert!(state.fields.get("UserId").is_none());
    }

    #[test]
    fn colliding_declared_aliases_fail_closed() {
        let mut table = work_summary_table();
        table.action_params.insert(
            "AttachVector".to_string(),
            BTreeSet::from(["user_id".to_string(), "UserId".to_string()]),
        );
        let params = serde_json::json!({ "UserId": "ambiguous" });
        assert!(matches!(
            undeclared_param_keys(&table, "AttachVector", &params),
            Err(ParamContractError::DeclarationAliasCollision { .. })
        ));

        let mut entity_state = state("WorkSummary");
        let result = process_action(&mut entity_state, &table, "AttachVector", &params);
        assert!(!result.success);
        assert!(entity_state.fields.get("UserId").is_none());
    }
}
