//! Exact state-timeout clock validation.

use temper_runtime::tenant::TenantId;

use super::super::super::types::ReactionRule;
use super::TimeoutClockStatus;

pub(super) async fn validate_timeout_clock(
    state: &crate::ServerState,
    store: &crate::storage::BoxedEventStore,
    intent: &crate::trigger::delivery::PersistedReactionIntent,
    rule: &ReactionRule,
) -> TimeoutClockStatus {
    use crate::trigger::delivery::{
        STATE_TIMEOUT_CLOCK_AUDIT_BUDGET, state_timeout_declaration_id, transition_table_digest,
    };

    let Some(clock) = intent.state_timeout.as_ref() else {
        return TimeoutClockStatus::Current(intent.source_sequence);
    };
    if clock.clock_sequence != intent.source_sequence || clock.state != intent.source_to_state {
        return TimeoutClockStatus::Rejected(
            "state-timeout clock does not match its committed source event".to_string(),
        );
    }
    if intent.not_before.is_none() {
        return TimeoutClockStatus::Rejected(
            "state-timeout intent is missing its absolute deadline".to_string(),
        );
    }

    if intent
        .schema_pin
        .as_ref()
        .is_some_and(|pin| pin.execution.bundle_digest != clock.schema_digest)
    {
        return TimeoutClockStatus::Rejected(
            "state-timeout scoped schema digest does not match committed clock".to_string(),
        );
    }

    let tenant = TenantId::new(&intent.tenant);
    if let Some(pin) = intent.schema_pin.as_ref() {
        let Some(deployment_store) = state.schema_deployment_store() else {
            return TimeoutClockStatus::Rejected(
                "state-timeout scoped migration status is unavailable".to_string(),
            );
        };
        let active = match deployment_store
            .active_schema_pointer(&intent.tenant, &pin.execution.scope)
            .await
        {
            Ok(pointer) => pointer,
            Err(error) => {
                return TimeoutClockStatus::Rejected(format!(
                    "state-timeout scoped migration check failed: {error}"
                ));
            }
        };
        if let Some(active) =
            active.filter(|active| active.bundle_digest != pin.execution.bundle_digest)
        {
            let active_pin = temper_runtime::persistence::schema_deployment::SchemaExecutionPin {
                scope: pin.execution.scope.clone(),
                bundle_digest: active.bundle_digest,
            };
            let migrated_id = format!(
                "{}:{}:{}",
                intent.tenant,
                intent.source_entity_type,
                temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
                    &intent.source_entity_id,
                    &active_pin,
                )
            );
            let migrated = match store.read_events_limited(&migrated_id, 0, 1).await {
                Ok(events) => events.into_iter().next().is_some_and(|envelope| {
                    envelope.event_type == crate::entity_actor::types::FIELD_UPDATE_EVENT_TYPE
                        && serde_json::from_value::<crate::entity_actor::EntityEvent>(
                            envelope.payload,
                        )
                        .ok()
                        .and_then(|event| {
                            event
                                .params
                                .get("migration")
                                .and_then(|value| value.as_bool())
                                .map(|migration| (event, migration))
                        })
                        .is_some_and(|(_, migration)| migration)
                }),
                Err(error) => {
                    return TimeoutClockStatus::Rejected(format!(
                        "state-timeout migration target audit failed: {error}"
                    ));
                }
            };
            if migrated {
                return TimeoutClockStatus::Superseded(
                    "state-timeout source entity was migrated to a successor schema".to_string(),
                );
            }
        }
    }
    let current = match intent.schema_pin.as_ref() {
        Some(pin) => {
            state
                .get_scoped_entity_state(
                    &tenant,
                    &intent.source_entity_type,
                    &intent.source_entity_id,
                    pin.execution.clone(),
                )
                .await
        }
        None => {
            state
                .get_tenant_entity_state(
                    &tenant,
                    &intent.source_entity_type,
                    &intent.source_entity_id,
                )
                .await
        }
    };
    let current = match current {
        Ok(response) => response,
        Err(error) => {
            return TimeoutClockStatus::Rejected(format!(
                "state-timeout source state could not be recovered: {error}"
            ));
        }
    };
    if current.state.status != clock.state {
        return TimeoutClockStatus::Superseded(format!(
            "state-timeout source left expected state '{}'",
            clock.state
        ));
    }

    let table = state.registry.read().ok().and_then(|registry| {
        intent.schema_pin.as_ref().map_or_else(
            || {
                registry
                    .get_spec(&tenant, &intent.source_entity_type)
                    .map(|spec| spec.table())
            },
            |pin| {
                registry
                    .get_scoped_spec_at_digest(
                        &tenant,
                        &pin.execution.scope,
                        &pin.execution.bundle_digest,
                        &intent.source_entity_type,
                    )
                    .map(|spec| spec.table())
            },
        )
    });
    let Some(table) = table else {
        return TimeoutClockStatus::Rejected(
            "state-timeout exact schema could not be recovered".to_string(),
        );
    };
    let actual_digest = intent.schema_pin.as_ref().map_or_else(
        || transition_table_digest(&table).ok(),
        |pin| Some(pin.execution.bundle_digest.clone()),
    );
    if actual_digest.as_deref() != Some(clock.schema_digest.as_str()) {
        return TimeoutClockStatus::Rejected(
            "state-timeout schema changed after the clock committed".to_string(),
        );
    }
    let declaration = table
        .state_timeouts
        .iter()
        .enumerate()
        .find(|(index, declaration)| {
            state_timeout_declaration_id(
                &clock.schema_digest,
                &intent.source_entity_type,
                *index,
                declaration,
            )
            .as_deref()
                == Ok(clock.declaration_id.as_str())
        })
        .map(|(_, declaration)| declaration);
    let Some(declaration) = declaration else {
        return TimeoutClockStatus::Rejected(
            "state-timeout declaration identity is absent from the exact schema".to_string(),
        );
    };
    let expected_deadline = i64::try_from(declaration.after_seconds)
        .ok()
        .and_then(|seconds| {
            intent
                .created_at
                .checked_add_signed(chrono::Duration::seconds(seconds))
        });
    let expected_params = serde_json::to_value(&declaration.params).ok();
    if declaration.state != clock.state
        || declaration.reset_on != clock.reset_on
        || declaration.max_occurrences != clock.max_occurrences
        || clock.occurrence_ordinal == 0
        || intent.not_before != expected_deadline
        || rule.then.entity_type != intent.source_entity_type
        || rule.then.action != declaration.on_timeout
        || expected_params.as_ref() != Some(&rule.then.params)
    {
        return TimeoutClockStatus::Rejected(
            "state-timeout intent does not match its exact declaration".to_string(),
        );
    }
    let persistence_id = match intent.schema_pin.as_ref() {
        Some(pin) => format!(
            "{}:{}:{}",
            intent.tenant,
            intent.source_entity_type,
            temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
                &intent.source_entity_id,
                &pin.execution,
            )
        ),
        None => format!(
            "{}:{}:{}",
            intent.tenant, intent.source_entity_type, intent.source_entity_id
        ),
    };
    let committed_occurrences = current.state.state_timeout_occurrences(&clock.state);
    if clock.occurrence_ordinal != committed_occurrences.saturating_add(1) {
        return TimeoutClockStatus::Rejected(
            "state-timeout occurrence ordinal does not follow durable receipt evidence".to_string(),
        );
    }
    if clock.occurrence_ordinal > u64::from(clock.max_occurrences) {
        return TimeoutClockStatus::Superseded(
            "state-timeout occurrence budget exhausted".to_string(),
        );
    }

    let later = match store
        .read_events_limited(
            &persistence_id,
            intent.source_sequence,
            STATE_TIMEOUT_CLOCK_AUDIT_BUDGET.saturating_add(1),
        )
        .await
    {
        Ok(events) => events,
        Err(error) => {
            return TimeoutClockStatus::Rejected(format!(
                "state-timeout clock audit failed: {error}"
            ));
        }
    };
    if later.len() > STATE_TIMEOUT_CLOCK_AUDIT_BUDGET {
        return TimeoutClockStatus::Rejected(
            "state-timeout clock audit budget exhausted".to_string(),
        );
    }
    for envelope in later {
        let event =
            match serde_json::from_value::<crate::entity_actor::EntityEvent>(envelope.payload) {
                Ok(event) => event,
                Err(error) => {
                    return TimeoutClockStatus::Rejected(format!(
                        "state-timeout clock audit found malformed event: {error}"
                    ));
                }
            };
        if event.from_status != event.to_status
            || clock.reset_on.iter().any(|action| action == &event.action)
        {
            return TimeoutClockStatus::Superseded(format!(
                "state-timeout clock was superseded by source event {}",
                envelope.sequence_nr
            ));
        }
    }
    TimeoutClockStatus::Current(current.state.sequence_nr)
}
