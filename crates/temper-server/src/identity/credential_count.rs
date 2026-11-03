//! Which credentials of one agent instance are Active, read from the query
//! plane's catalog so that no credential's actor is loaded to answer.

use serde_json::Value;
use temper_runtime::tenant::TenantId;

use crate::state::ServerState;
use crate::storage::{CatalogRowsLoad, load_catalog_rows_by_id};

impl ServerState {
    /// The ids (key hashes) of the tenant's Active `AgentCredential`s whose
    /// `agent_instance_id` is `agent_instance_id`, as the catalog last
    /// projected them: a credential issued moments ago may not be there yet.
    /// `None` when the store cannot answer.
    pub async fn active_credential_ids(
        &self,
        tenant: &TenantId,
        agent_instance_id: &str,
    ) -> Option<Vec<String>> {
        let query_plane = self.query_plane_store()?;
        let ids = self.list_entity_ids_lazy(tenant, "AgentCredential").await;
        if ids.is_empty() {
            return Some(Vec::new());
        }
        let rows =
            match load_catalog_rows_by_id(&query_plane, tenant.as_str(), "AgentCredential", &ids)
                .await
            {
                Ok(CatalogRowsLoad::Available(rows)) => rows,
                Ok(CatalogRowsLoad::Unsupported) | Err(_) => return None,
            };
        Some(
            rows.into_values()
                .filter(|row| {
                    row.status == "Active"
                        && row.fields.get("agent_instance_id").and_then(Value::as_str)
                            == Some(agent_instance_id)
                })
                .map(|row| row.entity_id)
                .collect(),
        )
    }
}
