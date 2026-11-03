//! Reading a type's stored records in some states, grouped by the values the
//! check needs.

use std::collections::{BTreeMap, BTreeSet};

use temper_runtime::tenant::TenantId;

use crate::entity_actor::EntityState;
use crate::state::ServerState;
use crate::storage::CatalogGroup;

impl ServerState {
    /// The stored records of `entity_type` in one of `statuses`, grouped by
    /// status and the values of `names` they hold, with how many records
    /// could not be read. One grouped catalog query over `every_state` covers
    /// each record whose catalog row is current and unwrapped, and so says
    /// itself which records it covers; each other record is read on its own.
    pub(super) async fn record_groups(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        names: &[String],
        statuses: &BTreeSet<String>,
        every_state: &BTreeSet<String>,
    ) -> Result<(Vec<CatalogGroup>, usize), String> {
        self.ensure_type_indexed(tenant, entity_type).await?;
        let ids: BTreeSet<String> = self
            .list_entity_ids(tenant, entity_type)
            .into_iter()
            .collect();
        if ids.is_empty() {
            return Ok((Vec::new(), 0));
        }
        let mut groups = Vec::new();
        let mut covered = BTreeSet::new();
        if let Some(store) = self.query_plane_store() {
            // Coverage from a second query could disagree with this one: a
            // row that catches up between them would be in neither.
            let every_state: Vec<String> = every_state.iter().cloned().collect();
            let found = store
                .catalog_groups(tenant.as_str(), entity_type, names, &every_state)
                .await
                .map_err(|error| error.to_string())?;
            for group in found.into_iter().flatten() {
                covered.extend(group.ids.iter().cloned());
                if statuses.contains(&group.status) {
                    groups.push(group);
                }
            }
        }
        for group in &mut groups {
            group.ids.retain(|id| ids.contains(id));
        }
        groups.retain(|group| !group.ids.is_empty());
        let mut unreadable = 0;
        for id in ids.iter().filter(|id| !covered.contains(*id)) {
            match self.current_record_state(tenant, entity_type, id).await {
                Some(state) if state.total_event_count > 0 && statuses.contains(&state.status) => {
                    groups.push(CatalogGroup {
                        status: state.status.clone(),
                        values: stored_values(&state, names),
                        ids: vec![id.clone()],
                    });
                }
                Some(_) => {}
                None => unreadable += 1,
            }
        }
        Ok((groups, unreadable))
    }
}

/// What a stored record holds of `names`, as its catalog row holds them.
fn stored_values(state: &EntityState, names: &[String]) -> BTreeMap<String, serde_json::Value> {
    names
        .iter()
        .filter_map(|name| {
            let value = state
                .counters
                .get(name)
                .map(|count| serde_json::Value::from(*count as u64))
                .or_else(|| {
                    state
                        .booleans
                        .get(name)
                        .map(|flag| serde_json::Value::from(*flag))
                })
                .or_else(|| state.lists.get(name).map(|items| serde_json::json!(items)))
                .or_else(|| state.fields.get(name).cloned())?;
            Some((name.clone(), value))
        })
        .collect()
}
