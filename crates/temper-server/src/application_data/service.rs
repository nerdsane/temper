//! Shared transport-neutral entity mutation service.

use std::collections::BTreeMap;

use temper_authz::{AuthzDenial, SecurityContext};
use temper_runtime::tenant::TenantId;

use crate::entity_actor::EntityResponse;
use crate::request_context::AgentContext;
use crate::state::{DispatchError, DispatchExtOptions, ServerState};
use crate::storage::{QueryFieldIndexOrder, QueryFieldIndexPage};

/// Governed entity data substrate shared by OData adapters and module invocations.
pub(crate) struct GovernedApplicationDataService<'a> {
    state: &'a ServerState,
}

impl<'a> GovernedApplicationDataService<'a> {
    /// Bind the service to server state for one adapter call.
    pub(crate) fn new(state: &'a ServerState) -> Self {
        Self { state }
    }

    /// Apply the adapter-neutral Cedar decision for a resolved resource.
    pub(crate) fn authorize(
        &self,
        tenant: &TenantId,
        security: &SecurityContext,
        action: &str,
        entity_type: &str,
        attributes: &BTreeMap<String, serde_json::Value>,
    ) -> Result<(), AuthzDenial> {
        self.state.authorize_with_context(
            security,
            action,
            entity_type,
            attributes,
            tenant.as_str(),
        )
    }

    /// Read one canonical actor state.
    #[tracing::instrument(skip_all, fields(otel.name = "application_data.service.get", entity_type))]
    pub(crate) async fn get(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<EntityResponse, String> {
        if !self.state.entity_exists(tenant, entity_type, entity_id)
            && !self
                .state
                .ensure_entity_loaded(tenant, entity_type, entity_id)
                .await
        {
            return Err("EntityNotFound".into());
        }
        self.state
            .get_tenant_entity_state(tenant, entity_type, entity_id)
            .await
    }

    /// Read a stable ordered page from the query plane when available.
    pub(crate) async fn query_candidates(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        order: &[QueryFieldIndexOrder],
        offset: usize,
        limit: usize,
    ) -> Result<Option<Vec<String>>, String> {
        self.query_index_page(
            tenant,
            entity_type,
            "TRUE",
            Vec::new(),
            order,
            offset,
            limit,
            false,
        )
        .await
        .map(|page| page.map(|page| page.entity_ids))
    }

    /// Execute one bounded query-plane page for either external adapter.
    #[expect(
        clippy::too_many_arguments,
        reason = "the query-plane boundary preserves its storage contract explicitly"
    )]
    pub(crate) async fn query_index_page(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        where_clause: &str,
        params: Vec<String>,
        order: &[QueryFieldIndexOrder],
        offset: usize,
        limit: usize,
        include_count: bool,
    ) -> Result<Option<QueryFieldIndexPage>, String> {
        let Some(store) = self.state.query_plane_store() else {
            return Ok(None);
        };
        store
            .query_field_index_page(
                tenant.as_str(),
                entity_type,
                where_clause,
                params,
                order,
                offset,
                limit,
                include_count,
            )
            .await
            .map_err(|error| error.to_string())
    }

    /// Enumerate the authoritative actor index for a bounded query fallback.
    pub(crate) async fn fallback_candidates(
        &self,
        tenant: &TenantId,
        entity_type: &str,
    ) -> Vec<String> {
        let mut ids = self.state.list_entity_ids_lazy(tenant, entity_type).await;
        ids.sort();
        ids
    }

    /// Enumerate only when authoritative completeness is already bounded.
    pub(crate) fn bounded_fallback_candidates(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        budget: usize,
    ) -> Result<Vec<String>, String> {
        self.state
            .list_entity_ids_bounded(tenant, entity_type, budget)
            .ok_or_else(|| "BoundedAuthoritativeFallbackUnavailable".into())
    }

    /// Read the currently indexed IDs for bounded projection-coverage repair.
    pub(crate) fn indexed_candidates(&self, tenant: &TenantId, entity_type: &str) -> Vec<String> {
        self.state.list_entity_ids(tenant, entity_type)
    }

    /// Create through the common durable actor/data-only path.
    #[tracing::instrument(skip_all, fields(otel.name = "application_data.service.create", entity_type))]
    pub(crate) async fn create(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        fields: serde_json::Value,
    ) -> Result<EntityResponse, String> {
        if let Some(response) = self
            .state
            .try_create_data_only_tenant_entity(tenant, entity_type, entity_id, fields.clone())
            .await?
        {
            return Ok(response);
        }
        self.state
            .get_or_create_tenant_entity(tenant, entity_type, entity_id, fields)
            .await
    }

    /// Create through the immutable task-scoped actor path.
    pub(crate) async fn create_scoped(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        fields: serde_json::Value,
        schema_pin: temper_runtime::persistence::schema_deployment::SchemaExecutionPin,
    ) -> Result<EntityResponse, String> {
        self.state
            .get_or_create_scoped_entity(tenant, entity_type, entity_id, fields, schema_pin)
            .await
    }

    /// Patch through the common exact-sequence actor path.
    #[tracing::instrument(skip_all, fields(otel.name = "application_data.service.patch", entity_type))]
    pub(crate) async fn patch(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        fields: serde_json::Value,
        expected_sequence: Option<u64>,
    ) -> Result<EntityResponse, String> {
        self.write_fields(
            tenant,
            entity_type,
            entity_id,
            fields,
            false,
            expected_sequence,
        )
        .await
    }

    /// Patch through one immutable task-scoped actor.
    #[expect(dead_code, reason = "reserved for the typed scoped-data adapter")]
    pub(crate) async fn patch_scoped(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        fields: serde_json::Value,
        expected_sequence: Option<u64>,
        schema_pin: temper_runtime::persistence::schema_deployment::SchemaExecutionPin,
    ) -> Result<EntityResponse, String> {
        self.state
            .update_scoped_entity_fields_if_sequence(
                tenant,
                entity_type,
                entity_id,
                fields,
                false,
                expected_sequence,
                schema_pin,
            )
            .await
    }

    /// Replace through the common actor path used by the OData adapter.
    #[expect(dead_code, reason = "reserved for the typed module-data adapter")]
    pub(crate) async fn replace(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        fields: serde_json::Value,
    ) -> Result<EntityResponse, String> {
        self.write_fields(tenant, entity_type, entity_id, fields, true, None)
            .await
    }

    /// Replace through one immutable task-scoped actor.
    #[expect(dead_code, reason = "reserved for the typed scoped-data adapter")]
    pub(crate) async fn replace_scoped(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        fields: serde_json::Value,
        schema_pin: temper_runtime::persistence::schema_deployment::SchemaExecutionPin,
    ) -> Result<EntityResponse, String> {
        self.state
            .update_scoped_entity_fields_if_sequence(
                tenant,
                entity_type,
                entity_id,
                fields,
                true,
                None,
                schema_pin,
            )
            .await
    }

    async fn write_fields(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        fields: serde_json::Value,
        replace: bool,
        expected_sequence: Option<u64>,
    ) -> Result<EntityResponse, String> {
        self.state
            .update_tenant_entity_fields_if_sequence(
                tenant,
                entity_type,
                entity_id,
                fields,
                replace,
                expected_sequence,
            )
            .await
    }

    /// Invoke an action through the common actor dispatch path.
    #[tracing::instrument(skip_all, fields(otel.name = "application_data.service.action", entity_type, action))]
    pub(crate) async fn action(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        params: serde_json::Value,
        agent: &AgentContext,
    ) -> Result<EntityResponse, String> {
        self.action_with_options(tenant, entity_type, entity_id, action, params, agent, true)
            .await
            .map_err(|error| error.to_string())
    }

    /// Invoke an action with the adapter's integration-wait preference.
    #[expect(
        clippy::too_many_arguments,
        reason = "the action boundary preserves the actor dispatch contract explicitly"
    )]
    pub(crate) async fn action_with_options(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        params: serde_json::Value,
        agent: &AgentContext,
        await_integration: bool,
    ) -> Result<EntityResponse, DispatchError> {
        self.state
            .dispatch_tenant_action_ext_typed(
                tenant,
                entity_type,
                entity_id,
                action,
                params,
                DispatchExtOptions {
                    agent_ctx: agent,
                    await_integration,
                    await_reactions: true,
                },
            )
            .await
    }
}
