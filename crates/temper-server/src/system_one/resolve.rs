//! Pre-transition resolution of System One guards in the common dispatch path.

use std::collections::BTreeMap;

use serde_json::{Value, json};
use temper_jit::table::TransitionTable;
use temper_runtime::tenant::TenantId;
use temper_spec::automaton::SystemOneGuard;

use super::evidence::json_digest;
use super::{
    ReceiptBinding, SystemOneEvidence, collect_guards, recorded_receipt, resolve_receipt,
    table_digest,
};
use crate::entity_actor::EntityState;
use crate::entity_actor::effects::{
    build_eval_context_with_xref, entity_authorization_precondition,
};
use crate::request_context::AgentContext;
use crate::state::{DispatchError, ServerState};

/// Bind authenticated authority independently of caller-supplied agent metadata.
/// Correlation and session metadata do not participate in this stable identity.
pub(crate) fn canonical_principal_identity(agent_ctx: &AgentContext) -> Result<String, String> {
    let value = match &agent_ctx.security_ctx {
        Some(context) => {
            let principal = &context.principal;
            let attributes: BTreeMap<_, _> = principal.attributes.iter().collect();
            json!({
                "kind": principal.kind,
                "id": principal.id,
                "role": principal.role,
                "acting_for": principal.acting_for,
                "agent_type": principal.agent_type,
                "attributes": attributes,
            })
        }
        None => json!({
            "kind": "Customer", "id": "anonymous", "role": null,
            "acting_for": null, "agent_type": null, "attributes": {},
        }),
    };
    json_digest(&value)
}

fn entity_projection(state: &EntityState) -> Value {
    let mut entity = state.fields.as_object().cloned().unwrap_or_default();
    for (name, value) in &state.counters {
        entity.insert(name.clone(), json!(value));
    }
    for (name, value) in &state.booleans {
        entity.insert(name.clone(), json!(value));
    }
    for (name, value) in &state.lists {
        entity.insert(name.clone(), json!(value));
    }
    let mut entity = Value::Object(entity);
    crate::entity_actor::effects::canonicalize_entity_fields(
        &mut entity,
        &state.entity_id,
        &state.status,
    );
    entity
}

async fn resolve_context(
    guard: &SystemOneGuard,
    entity: &Value,
    params: &Value,
    source: &crate::blobs::BlobReadSource<'_>,
) -> Result<Value, String> {
    let mut selected = serde_json::Map::new();
    for (source, field) in guard.state_references()? {
        if source == "entity" {
            let value = entity
                .get(&field)
                .ok_or_else(|| format!("missing system_one state reference 'entity.{field}'"))?;
            selected.insert(field, value.clone());
        }
    }
    let mut selected = Value::Object(selected);
    let budget = crate::blobs::BlobHydrationBudget::new(
        super::provider::SYSTEM_ONE_REQUEST_BYTE_BUDGET,
        super::provider::SYSTEM_ONE_REQUEST_BYTE_BUDGET,
        0,
        0,
    );
    crate::blobs::hydrate_comparison_fields_with_budget(source, &mut selected, &budget)
        .await
        .map_err(|_| {
            "system_one context blob is unavailable, invalid, or exceeds its byte budget"
                .to_string()
        })?;
    guard.resolve_state(&selected, params)
}

async fn validate_action_input(
    table: &TransitionTable,
    state: &EntityState,
    action: &str,
    params: &Value,
    source: &crate::blobs::BlobReadSource<'_>,
) -> Result<(), String> {
    let mut fields = serde_json::Map::new();
    if let Some(contract) = table.action_contracts.get(action) {
        for name in contract
            .constraints
            .iter()
            .filter_map(|constraint| constraint.field())
        {
            if !state.counters.contains_key(name)
                && !state.booleans.contains_key(name)
                && let Some(value) = state.fields.get(name)
            {
                fields.insert(name.to_string(), value.clone());
            }
        }
    }
    let mut fields = Value::Object(fields);
    crate::blobs::hydrate_comparison_fields(source, &mut fields).await?;
    table.validate_action_params(action, params, &fields, &state.counters, &state.booleans)
}

pub(crate) struct SystemOneResolution<'a> {
    pub tenant: &'a TenantId,
    pub table: &'a TransitionTable,
    pub state: &'a EntityState,
    pub action: &'a str,
    pub params: &'a Value,
    pub agent_ctx: &'a AgentContext,
    pub attempt_id: &'a str,
    pub cross_entity_booleans: &'a BTreeMap<String, bool>,
}

impl ServerState {
    pub(crate) async fn resolve_system_one_guards(
        &self,
        input: SystemOneResolution<'_>,
    ) -> Result<Option<SystemOneEvidence>, DispatchError> {
        let SystemOneResolution {
            tenant,
            table,
            state,
            action,
            params,
            agent_ctx,
            attempt_id,
            cross_entity_booleans,
        } = input;
        let guards = collect_guards(table, action);
        if guards.is_empty() {
            return Ok(None);
        }
        if table.composite_actions.contains_key(action)
            || self.is_pg_actor_backed(tenant, &state.entity_type)
        {
            return Err(DispatchError::Internal(
                "system_one guards are not supported by composite or Postgres actor execution"
                    .into(),
            ));
        }
        validate_action_input(
            table,
            state,
            action,
            params,
            &crate::blobs::BlobReadSource::Tenant {
                state: self,
                tenant,
            },
        )
        .await
        .map_err(DispatchError::Conflict)?;
        let mut ctx = build_eval_context_with_xref(state, cross_entity_booleans);
        for guard in &guards {
            ctx.booleans.insert(guard.key(), true);
        }
        if !table
            .evaluate_ctx(&state.status, &ctx, action)
            .is_some_and(|r| r.success)
        {
            return Err(DispatchError::Conflict(
                "ordinary action preconditions are not satisfied; system_one was not evaluated"
                    .into(),
            ));
        }
        let (store, _) = self.event_journal().ok_or_else(|| {
            DispatchError::Internal("system_one guards require durable event storage".into())
        })?;
        let entity = entity_projection(state);
        self.authorize_guarded_action(tenant, state, action, &entity, agent_ctx)
            .await?;
        self.authorize_system_one(tenant, agent_ctx)?;
        let expected_state = entity_authorization_precondition(state);
        let expected_table = table_digest(table).map_err(DispatchError::Internal)?;
        let params_digest = json_digest(params).map_err(DispatchError::Internal)?;
        let principal = canonical_principal_identity(agent_ctx).map_err(DispatchError::Internal)?;
        super::bind_attempt(
            &store,
            super::AttemptInput {
                tenant,
                table,
                state,
                action,
                params,
                principal: &principal,
                attempt: attempt_id,
            },
        )
        .await
        .map_err(DispatchError::Conflict)?;
        let mut evidence = SystemOneEvidence {
            receipt_ids: Vec::new(),
            expected_state: expected_state.clone(),
            expected_table: expected_table.clone(),
            attempt_id: attempt_id.to_owned(),
            params_digest: params_digest.clone(),
            outcomes: BTreeMap::new(),
        };
        for (guard_slot, guard) in guards.into_iter().enumerate() {
            let binding = ReceiptBinding {
                tenant: tenant.to_string(),
                principal: principal.clone(),
                entity_type: state.entity_type.clone(),
                entity_id: state.entity_id.clone(),
                action: action.to_owned(),
                attempt_id: attempt_id.to_owned(),
                expected_state: expected_state.clone(),
                expected_table: expected_table.clone(),
                params_digest: params_digest.clone(),
                guard_key: guard.key(),
                guard_slot,
            };
            let (id, receipt) = if let Some(saved) = recorded_receipt(&store, guard, &binding)
                .await
                .map_err(DispatchError::Conflict)?
            {
                saved
            } else {
                let provider = self.system_one_provider.as_ref().ok_or_else(|| {
                    DispatchError::Internal(
                        "system_one evaluation provider is not configured".into(),
                    )
                })?;
                let resolved_state = resolve_context(
                    guard,
                    &entity,
                    params,
                    &crate::blobs::BlobReadSource::Tenant {
                        state: self,
                        tenant,
                    },
                )
                .await
                .map_err(DispatchError::Internal)?;
                let request = guard.request(resolved_state);
                resolve_receipt(&store, provider.as_ref(), guard, binding, request)
                    .await
                    .map_err(DispatchError::Conflict)?
            };
            if let Some(error) = receipt.error {
                return Err(DispatchError::Internal(error));
            }
            if !receipt.passed {
                return Err(DispatchError::Conflict(format!(
                    "system_one assertion not satisfied: {}",
                    guard.assertion
                )));
            }
            evidence.outcomes.insert(guard.key(), receipt.passed);
            evidence.receipt_ids.push(id);
        }
        // Policies may have changed while external inference was in flight.
        self.authorize_system_one(tenant, agent_ctx)?;
        self.authorize_guarded_action(tenant, state, action, &entity, agent_ctx)
            .await?;
        Ok(Some(evidence))
    }

    async fn authorize_guarded_action(
        &self,
        tenant: &TenantId,
        state: &EntityState,
        action: &str,
        entity: &Value,
        agent_ctx: &AgentContext,
    ) -> Result<(), DispatchError> {
        let security_ctx = agent_ctx.security_ctx.as_ref().ok_or_else(|| {
            DispatchError::AuthzDenied("system_one guards require a caller security context".into())
        })?;
        let attrs = self
            .build_authz_resource_attrs(
                tenant,
                &state.entity_type,
                &state.entity_id,
                &state.status,
                entity,
            )
            .await
            .map_err(DispatchError::Internal)?;
        self.authorize_with_context(
            security_ctx,
            action,
            &state.entity_type,
            &attrs,
            tenant.as_str(),
        )
        .map_err(|error| DispatchError::AuthzDenied(error.to_string()))
    }

    fn authorize_system_one(
        &self,
        tenant: &TenantId,
        agent_ctx: &AgentContext,
    ) -> Result<(), DispatchError> {
        let mut security_ctx = agent_ctx.security_ctx.clone().ok_or_else(|| {
            DispatchError::AuthzDenied("system_one guards require a caller security context".into())
        })?;
        security_ctx
            .context_attrs
            .insert("method".into(), Value::String("POST".into()));
        security_ctx.context_attrs.insert(
            "url".into(),
            Value::String(super::provider::TYPESAFE_ENDPOINT.into()),
        );
        let attrs = BTreeMap::from([
            ("id".into(), Value::String("api.typesafe.ai".into())),
            ("domain".into(), Value::String("api.typesafe.ai".into())),
        ]);
        self.authorize_with_context(
            &security_ctx,
            "http_call",
            "HttpEndpoint",
            &attrs,
            tenant.as_str(),
        )
        .map_err(|e| DispatchError::AuthzDenied(e.to_string()))?;
        let attrs = BTreeMap::from([
            ("id".into(), Value::String("TYPESAFE_API_KEY".into())),
            ("key_name".into(), Value::String("TYPESAFE_API_KEY".into())),
        ]);
        self.authorize_with_context(
            &security_ctx,
            "access_secret",
            "Secret",
            &attrs,
            tenant.as_str(),
        )
        .map_err(|e| DispatchError::AuthzDenied(e.to_string()))
    }
}

#[cfg(test)]
#[path = "resolve_test.rs"]
mod tests;
