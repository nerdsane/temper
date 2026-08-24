//! Durable state-timeout intent identity and normalization (ADR-0178).

use chrono::Duration;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use temper_runtime::persistence::schema_deployment::SchemaEventPin;

use super::{PersistedReactionIntent, stable_delivery_id};

/// Named service principal that owns durable state-timeout delivery.
pub const STATE_TIMEOUT_SERVICE: &str = "timeout-scheduler";
/// Maximum later source events inspected while proving a timeout clock is current.
pub const STATE_TIMEOUT_CLOCK_AUDIT_BUDGET: usize = 10_000;

/// Kernel delivery category sharing the durable reaction lifecycle.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryKind {
    /// An authored cross-entity reaction.
    #[default]
    Reaction,
    /// A generated same-entity state-entry timeout.
    StateTimeout,
}

impl DeliveryKind {
    /// Stable low-cardinality label used by delivery telemetry.
    pub(crate) const fn metric_label(self) -> &'static str {
        match self {
            Self::Reaction => "reaction",
            Self::StateTimeout => "state_timeout",
        }
    }
}

/// Immutable evidence that must still describe the active timeout clock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateTimeoutPrecondition {
    /// Stable declaration identity under the exact schema digest.
    pub declaration_id: String,
    /// State whose entry/reset fixed the deadline.
    pub state: String,
    /// Source sequence that fixed the active clock.
    pub clock_sequence: u64,
    /// Exact global spec or scoped bundle digest.
    pub schema_digest: String,
    /// Same-state actions that supersede this clock.
    pub reset_on: Vec<String>,
    /// Total successful firings permitted across repeated entries.
    pub max_occurrences: u32,
    /// One-based firing ordinal derived from durable receipt evidence.
    pub occurrence_ordinal: u64,
}

/// Committed entity evidence used to normalize generated timeout intents.
pub struct StateTimeoutIntentContext<'a> {
    /// Tenant owning the authoritative entity journal.
    pub tenant: &'a str,
    /// Entity type containing the timeout declaration.
    pub entity_type: &'a str,
    /// Stable entity identifier.
    pub entity_id: &'a str,
    /// Committed source event sequence.
    pub source_sequence: u64,
    /// Event that established or reset the timeout clock.
    pub event: &'a crate::entity_actor::EntityEvent,
    /// Authoritative fields after the source event.
    pub source_fields: &'a serde_json::Value,
    /// Exact transition table used for the source event.
    pub table: &'a temper_jit::table::TransitionTable,
    /// Exact scoped schema pin, when the entity is scope-bound.
    pub schema_pin: Option<SchemaEventPin>,
    /// Cedar authority that established the clock, when request-derived.
    pub triggering_authority: Option<&'a serde_json::Value>,
    /// Durable timeout occurrence counters reconstructed from receipts.
    pub durable_idempotency_evidence: &'a std::collections::BTreeMap<String, u64>,
}

/// Build generated timeout intents to co-commit with one entity event.
///
/// Entry, bootstrap creation, and declared `reset_on` actions each establish
/// one new absolute clock. The exact declaration and schema identity are
/// snapshotted so later hot swaps cannot reinterpret committed scheduling.
pub fn state_timeout_intents(
    context: StateTimeoutIntentContext<'_>,
) -> Result<Vec<PersistedReactionIntent>, String> {
    let StateTimeoutIntentContext {
        tenant,
        entity_type,
        entity_id,
        source_sequence,
        event,
        source_fields,
        table,
        schema_pin,
        triggering_authority,
        durable_idempotency_evidence,
    } = context;
    if table.state_timeouts.is_empty() {
        return Ok(Vec::new());
    }
    let schema_digest = match schema_pin.as_ref() {
        Some(pin) => pin.execution.bundle_digest.clone(),
        None => transition_table_digest(table)?,
    };
    let authority = match triggering_authority {
        Some(authority) => authority.clone(),
        None => serde_json::to_value(
            crate::request_context::AgentContext::for_service(STATE_TIMEOUT_SERVICE)
                .security_ctx
                .expect("timeout service context must carry authority"),
        )
        .map_err(|error| error.to_string())?,
    };
    let state_changed = event.from_status != event.to_status;
    let mut intents = Vec::new();
    for (declaration_index, declaration) in table.state_timeouts.iter().enumerate() {
        if declaration.state != event.to_status {
            continue;
        }
        let resets_clock = state_changed
            || event.action == "Created"
            || declaration
                .reset_on
                .iter()
                .any(|action| action == &event.action);
        if !resets_clock {
            continue;
        }
        let seconds = i64::try_from(declaration.after_seconds)
            .map_err(|_| "state-timeout deadline exceeds scheduler range".to_string())?;
        let deadline = event
            .timestamp
            .checked_add_signed(Duration::seconds(seconds))
            .ok_or_else(|| "state-timeout deadline overflows scheduler clock".to_string())?;
        let declaration_id = state_timeout_declaration_id(
            &schema_digest,
            entity_type,
            declaration_index,
            declaration,
        )?;
        let occurrence_ordinal = durable_idempotency_evidence
            .get(&format!("state-timeout-occurrences:{}", declaration.state))
            .copied()
            .unwrap_or(0)
            .saturating_add(1);
        let trigger_name = format!("state-timeout:{declaration_id}");
        let delivery_id = stable_delivery_id(
            tenant,
            entity_type,
            entity_id,
            &event.action,
            source_sequence,
            &trigger_name,
            declaration_index,
        );
        let rule = crate::trigger::types::ReactionRule {
            name: trigger_name.clone(),
            when: crate::trigger::types::ReactionTrigger {
                entity_type: entity_type.to_string(),
                action: Some(event.action.clone()),
                to_state: Some(declaration.state.clone()),
                guard: None,
            },
            then: crate::trigger::types::ReactionTarget {
                entity_type: entity_type.to_string(),
                action: declaration.on_timeout.clone(),
                params: serde_json::to_value(&declaration.params)
                    .map_err(|error| error.to_string())?,
                params_from: std::collections::BTreeMap::new(),
            },
            resolve_target: crate::trigger::types::TargetResolver::SameId,
            // Transition/reset clocks retain the exact triggering authority.
            // Bootstrap creation has no request principal and therefore uses
            // the explicitly named timeout-scheduler authority above.
            principal: None,
            drop_ok: false,
        };
        intents.push(PersistedReactionIntent {
            kind: DeliveryKind::StateTimeout,
            root_delivery_id: delivery_id.clone(),
            delivery_id,
            tenant: tenant.to_string(),
            source_entity_type: entity_type.to_string(),
            source_entity_id: entity_id.to_string(),
            source_action: event.action.clone(),
            source_sequence,
            source_to_state: event.to_status.clone(),
            source_fields: source_fields.clone(),
            guard_passed: true,
            target_entity_id: Some(entity_id.to_string()),
            trigger_name,
            trigger_index: declaration_index,
            depth: 0,
            rule: serde_json::to_value(rule).map_err(|error| error.to_string())?,
            authority: authority.clone(),
            created_at: event.timestamp,
            not_before: Some(deadline),
            state_timeout: Some(StateTimeoutPrecondition {
                declaration_id,
                state: declaration.state.clone(),
                clock_sequence: source_sequence,
                schema_digest: schema_digest.clone(),
                reset_on: declaration.reset_on.clone(),
                max_occurrences: declaration.max_occurrences,
                occurrence_ordinal,
            }),
            schema_pin: schema_pin.clone(),
        });
    }
    Ok(intents)
}

/// Deterministic digest of the complete compiled global entity behavior.
pub fn transition_table_digest(
    table: &temper_jit::table::TransitionTable,
) -> Result<String, String> {
    if let Some(digest) = table.schema_digest.as_ref() {
        return Ok(digest.clone());
    }
    let encoded = serde_json::to_vec(table).map_err(|error| error.to_string())?;
    Ok(format!("sha256:{:x}", Sha256::digest(encoded)))
}

pub(crate) fn state_timeout_declaration_id(
    schema_digest: &str,
    entity_type: &str,
    declaration_index: usize,
    declaration: &temper_spec::automaton::StateTimeout,
) -> Result<String, String> {
    let encoded = serde_json::to_vec(declaration).map_err(|error| error.to_string())?;
    let mut digest = Sha256::new();
    for component in [schema_digest.as_bytes(), entity_type.as_bytes(), &encoded] {
        digest.update(component.len().to_be_bytes());
        digest.update(component);
    }
    digest.update(declaration_index.to_be_bytes());
    Ok(format!("state-timeout-v1-{:x}", digest.finalize()))
}
