//! Spec lookup and Cedar resource snapshots.

use super::{AuthzResourceSnapshot, FailedLevelInfo, VerificationGateError};
use crate::registry::VerificationStatus;
use crate::state::ServerState;
use std::collections::BTreeMap;
use temper_authz::{AuthzDecision, AuthzDenial, SecurityContext};
use temper_observe::wide_event;
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;
use tracing::instrument;

impl ServerState {
    /// Whether the tenant registry has a spec for this entity type.
    pub(crate) fn has_registered_spec(
        &self,
        tenant: &TenantId,
        entity_type: &str,
    ) -> Result<bool, String> {
        self.registry
            .read()
            .map(|registry| registry.get_spec(tenant, entity_type).is_some())
            .map_err(|e| format!("registry lock poisoned: {e}"))
    }

    /// Returns `true` when dispatch should be allowed for the entity type.
    ///
    /// This includes both tenant-scoped specs and legacy single-tenant
    /// transition tables.
    pub(crate) fn is_entity_type_governed(
        &self,
        tenant: &TenantId,
        entity_type: &str,
    ) -> Result<bool, String> {
        Ok(self.has_registered_spec(tenant, entity_type)?
            || self.transition_tables.contains_key(entity_type))
    }

    /// Declared `[[key]]` set for a `(tenant, entity_type)` (ADR-0153), resolved
    /// from the SAME sources dispatch uses: the per-tenant registry first — where
    /// runtime-installed os-app entities (File, Directory, SessionEntry, …) live —
    /// then the legacy single-tenant transition tables.
    ///
    /// The keyed read fast path MUST resolve keys through here. Reading
    /// `transition_tables` directly only sees the boot-time single-tenant set and
    /// silently omits every registry-installed entity, disabling the keyed path so
    /// point reads fall back to the budget-bounded scan and 413 at scale — the
    /// TemperFS root-directory failure in ARN-68.
    pub(crate) fn declared_keys_for(
        &self,
        tenant: &TenantId,
        entity_type: &str,
    ) -> Vec<temper_jit::table::types::DeclaredKey> {
        // Fail fast on a poisoned registry lock rather than silently falling through
        // to `transition_tables` — a silent fallback would re-introduce exactly the
        // ARN-68 bug (registry-installed keys not found → keyed path disabled → scan).
        {
            let registry = self.registry.read().expect("registry lock poisoned");
            if let Some(table) = registry.get_table(tenant, entity_type) {
                return table.keys.clone();
            }
        }
        self.transition_tables
            .get(entity_type)
            .map(|table| table.keys.clone())
            .unwrap_or_default()
    }

    /// The declared `[[vector]]` access paths for `(tenant, entity_type)` — the
    /// registry table first (covers os-app entities), the boot-time
    /// `transition_tables` as fallback (ADR-0155). Same registry-lock discipline as
    /// [`Self::declared_keys_for`]: fail fast on a poisoned lock rather than silently
    /// falling through. Empty when the type declares no vector path.
    pub(crate) fn declared_vectors_for(
        &self,
        tenant: &TenantId,
        entity_type: &str,
    ) -> Vec<temper_jit::table::types::DeclaredVector> {
        {
            let registry = self.registry.read().expect("registry lock poisoned");
            if let Some(table) = registry.get_table(tenant, entity_type) {
                return table.vectors.clone();
            }
        }
        self.transition_tables
            .get(entity_type)
            .map(|table| table.vectors.clone())
            .unwrap_or_default()
    }

    /// Load the current entity state and derive the Cedar resource view used
    /// for action authorization.
    pub(crate) async fn load_authz_resource_snapshot(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<AuthzResourceSnapshot, String> {
        let current_state = self
            .get_tenant_entity_state(tenant, entity_type, entity_id)
            .await?;

        let resource_attrs = self
            .build_authz_resource_attrs(
                tenant,
                entity_type,
                entity_id,
                &current_state.state.status,
                &current_state.state.fields,
            )
            .await?;

        Ok(AuthzResourceSnapshot {
            current_state,
            resource_attrs,
        })
    }

    /// Build the Cedar resource view for a prospective entity representation.
    ///
    /// Mutation handlers use this after applying PATCH/PUT fields so policies
    /// evaluate the state that would be committed, including refreshed context
    /// entity status attributes.
    pub(crate) async fn build_authz_resource_attrs(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        status: &str,
        fields: &serde_json::Value,
    ) -> Result<BTreeMap<String, serde_json::Value>, String> {
        let mut resource_attrs = BTreeMap::new();
        if let serde_json::Value::Object(fields) = fields {
            for (k, v) in fields {
                if !temper_spec::automaton::is_server_derived_field_name(k) {
                    resource_attrs.insert(k.clone(), v.clone());
                }
            }
        }

        for key in ["id", "Id"] {
            resource_attrs.insert(
                key.to_string(),
                serde_json::Value::String(entity_id.to_string()),
            );
        }
        for key in ["status", "Status"] {
            resource_attrs.insert(
                key.to_string(),
                serde_json::Value::String(status.to_string()),
            );
        }

        let context_entities: Vec<temper_spec::automaton::ContextEntityDecl> = self
            .registry
            .read()
            .map_err(|e| format!("registry lock poisoned: {e}"))?
            .get_spec(tenant, entity_type)
            .map(|s| s.automaton.context_entities.clone())
            .unwrap_or_default();

        for ce in &context_entities {
            let target_id = fields
                .get(&ce.id_field)
                .and_then(|v| v.as_str())
                .unwrap_or("");

            if !target_id.is_empty()
                && let Some(status) = self
                    .resolve_entity_status(tenant, &ce.entity_type, target_id)
                    .await
            {
                resource_attrs.insert(
                    format!("ctx_{}_status", ce.name),
                    serde_json::Value::String(status),
                );
            }
        }

        let has_spec = self.has_registered_spec(tenant, entity_type)?;
        resource_attrs.insert("has_spec".to_string(), serde_json::Value::Bool(has_spec));
        Ok(resource_attrs)
    }

    /// Return the spec-defined initial state used by a true entity create.
    pub(crate) fn initial_entity_status(
        &self,
        tenant: &TenantId,
        entity_type: &str,
    ) -> Result<String, String> {
        if let Some(table) = self
            .registry
            .read()
            .map_err(|error| format!("registry lock poisoned: {error}"))?
            .get_table(tenant, entity_type)
        {
            return Ok(table.initial_state.clone());
        }
        self.transition_tables
            .get(entity_type)
            .map(|table| table.initial_state.clone())
            .ok_or_else(|| {
                format!("No transition table for tenant '{tenant}', entity type '{entity_type}'")
            })
    }

    /// Build trusted Cedar attributes for a durably absent create target.
    pub(crate) async fn build_create_authz_resource_attrs(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        fields: &serde_json::Value,
    ) -> Result<BTreeMap<String, serde_json::Value>, String> {
        let initial_status = self.initial_entity_status(tenant, entity_type)?;
        self.build_authz_resource_attrs(tenant, entity_type, entity_id, &initial_status, fields)
            .await
    }

    /// Check authorization using a pre-built `SecurityContext`.
    ///
    /// Accepts `BTreeMap` for DST compliance; converts at the authz boundary.
    pub fn authorize_with_context(
        &self,
        security_ctx: &SecurityContext,
        action: &str,
        resource_type: &str,
        resource_attrs: &BTreeMap<String, serde_json::Value>,
        tenant: &str,
    ) -> Result<(), AuthzDenial> {
        let attrs: std::collections::HashMap<_, _> = resource_attrs // determinism-ok: Cedar API requires HashMap
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(); // determinism-ok
        let authz_start = sim_now();
        let decision = self.authz.authorize_for_tenant_or_bypass(
            tenant,
            security_ctx,
            action,
            resource_type,
            &attrs,
        );
        let duration_ns = (sim_now() - authz_start)
            .num_nanoseconds()
            .unwrap_or(0)
            .max(0) as u64;
        let decision_str = match &decision {
            AuthzDecision::Allow { .. } => "Allow",
            AuthzDecision::Deny(_) => "Deny",
        };
        // Correlate the decision with the resource it governed and the request
        // that triggered it. `resource_attrs["id"]` is the Cedar resource id
        // every caller populates (see `resource_attrs_from_body`); the trace id
        // comes from the active span, which is the same request span the HTTP
        // handler and the dispatch both run under.
        let entity_id = resource_attrs
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let trace_id = crate::request_context::current_span_trace_context_ids()
            .map(|(trace_id, _span_id)| trace_id)
            .unwrap_or_default();
        let wide = wide_event::from_authz_decision(wide_event::AuthzDecisionInput {
            action,
            resource_type,
            entity_id,
            principal_kind: &format!("{:?}", security_ctx.principal.kind),
            decision: decision_str,
            duration_ns,
            tenant,
            trace_id: &trace_id,
        });
        wide_event::emit_span(&wide);
        wide_event::emit_metrics(&wide);
        match decision {
            AuthzDecision::Allow { .. } => Ok(()),
            AuthzDecision::Deny(denial) => Err(denial),
        }
    }

    /// Get the current state of an entity actor (legacy single-tenant).
    pub fn enrich_metadata(&self, tenant: &TenantId, action_name: &str, hint: &str) {
        const AGENT_HINTS_BUDGET: usize = 1_000;
        let Ok(mut all_hints) = self.agent_hints.write() else {
            return;
        };
        let hints = all_hints.entry(tenant.clone()).or_default();
        hints.insert(action_name.to_string(), hint.to_string());
        while hints.len() > AGENT_HINTS_BUDGET {
            let oldest_key = hints.iter().next().map(|(k, _)| k.clone());
            if let Some(k) = oldest_key {
                hints.remove(&k);
            } else {
                break;
            }
        }
    }

    /// Check the verification gate for a specific entity type.
    ///
    /// Returns `Ok(())` if the entity type is verified and operations are allowed.
    /// Returns `Err(VerificationGateError)` if operations should be blocked.
    ///
    /// Policy:
    /// - `None` → `Ok(())` (backward compat for legacy single-tenant without registry)
    /// - `Pending` → `Err("pending")` — verification hasn't started yet
    /// - `Running` → `Err("running")` — verification is in progress
    /// - `Completed(all_passed: true)` → `Ok(())`
    /// - `Completed(all_passed: false)` → `Err("failed")` with failed level details
    #[instrument(skip_all, fields(otel.name = "entity.check_verification_gate", tenant = %tenant, entity_type))]
    pub fn check_verification_gate(
        &self,
        tenant: &TenantId,
        entity_type: &str,
    ) -> Result<(), VerificationGateError> {
        let registry = self.registry.read().unwrap();

        // If there's no tenant config in the registry, this is a legacy
        // single-tenant setup — allow operations for backward compatibility.
        let Some(tenant_config) = registry.get_tenant(tenant) else {
            return Ok(());
        };

        // If the entity type doesn't exist in the tenant, there's nothing to gate.
        if !tenant_config.entities.contains_key(entity_type) {
            return Ok(());
        }

        match tenant_config.verification.get(entity_type) {
            None => Ok(()),
            Some(VerificationStatus::Pending) => Err(VerificationGateError {
                entity_type: entity_type.to_string(),
                status: "pending".to_string(),
                message: format!(
                    "Verification has not started for entity type '{entity_type}'. \
                     Waiting for verification cascade to begin."
                ),
                failed_levels: None,
            }),
            Some(VerificationStatus::Running) => Err(VerificationGateError {
                entity_type: entity_type.to_string(),
                status: "running".to_string(),
                message: format!(
                    "Verification is currently running for entity type '{entity_type}'. \
                     Please wait for the cascade to complete."
                ),
                failed_levels: None,
            }),
            Some(VerificationStatus::Completed(result) | VerificationStatus::Restored(result)) => {
                if result.all_passed {
                    Ok(())
                } else {
                    let failed_levels: Vec<FailedLevelInfo> = result
                        .levels
                        .iter()
                        .filter(|l| !l.passed)
                        .map(|l| FailedLevelInfo {
                            level: l.level.clone(),
                            summary: l.summary.clone(),
                            details: l.details.clone(),
                        })
                        .collect();
                    Err(VerificationGateError {
                        entity_type: entity_type.to_string(),
                        status: "failed".to_string(),
                        message: format!(
                            "Verification failed for entity type '{entity_type}'. \
                             Fix the spec and re-push."
                        ),
                        failed_levels: Some(failed_levels),
                    })
                }
            }
        }
    }
}
