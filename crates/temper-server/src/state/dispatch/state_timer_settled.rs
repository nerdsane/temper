//! The records the boot walk can leave as they are, told from the catalog
//! without replaying their journals.
//!
//! By the time the boot walk runs, the boot repair has brought every lagging
//! catalog row up to its journal, and the boot has armed every timer row again
//! from the current spec. A record whose row was armed for the state it is in
//! now already has the timer the walk would give it, and a record in a state
//! with no timeout and no row has none to give. Replaying either changes
//! nothing, and replaying every record cost an app with a few hundred records
//! half a minute of every boot. So the walk replays only the rest: a record in
//! a timed state with no row, one whose row is for another state, and one the
//! catalog cannot vouch for.

use std::collections::{BTreeMap, BTreeSet};

use temper_runtime::tenant::TenantId;

use crate::state::ServerState;

/// The timer rows the boot armed again, by tenant, type and record, with the
/// state each was armed for.
pub(crate) type BootRows = BTreeMap<(TenantId, String, String), String>;

impl ServerState {
    /// The records of `entity_type` the boot walk need not replay: each whose
    /// catalog row is current with its journal and which either has a row in
    /// `rows` for the state it is in, or is in a state that declares no
    /// timeout and has no row. With no catalog, or one that cannot be read,
    /// none is settled and the walk replays every record, as it must.
    pub(super) async fn settled_records(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        rows: &BootRows,
    ) -> BTreeSet<String> {
        let Some(store) = self.query_plane_store() else {
            return BTreeSet::new();
        };
        let (states, timed): (Vec<String>, BTreeSet<String>) = {
            let Ok(registry) = self.registry.read() else {
                return BTreeSet::new();
            };
            let Some(spec) = registry.get_spec(tenant, entity_type) else {
                return BTreeSet::new();
            };
            (
                spec.automaton.automaton.states.clone(),
                spec.automaton
                    .state_timeouts
                    .iter()
                    .map(|timeout| timeout.state.clone())
                    .collect(),
            )
        };
        let groups = match store
            .catalog_groups(tenant.as_str(), entity_type, &[], &states)
            .await
        {
            Ok(groups) => groups,
            Err(error) => {
                tracing::warn!(tenant = %tenant, entity_type, %error,
                    "catalog not readable; the boot walk replays every record of the type");
                return BTreeSet::new();
            }
        };
        let mut settled = BTreeSet::new();
        for group in groups.into_iter().flatten() {
            for id in group.ids {
                let key = (tenant.clone(), entity_type.to_string(), id);
                let follows = match rows.get(&key) {
                    Some(armed_for) => *armed_for == group.status,
                    None => !timed.contains(&group.status),
                };
                if follows {
                    settled.insert(key.2);
                }
            }
        }
        settled
    }
}
