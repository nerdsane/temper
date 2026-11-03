//! The records that describe the caller to Cedar.
//!
//! A type marked `principal_record` (an app's `Member`) holds one record per
//! user, keyed by the user's `sub`. Its status reaches Cedar on the principal:
//! `principal.<Type>` when the user calls in person, `principal.actingFor<Type>`
//! when an agent calls for them, so a policy can tell the two apart. Both keys
//! come from the verified token alone. A type that names a list field as its
//! `principal_record_set` (the roles a person holds) also puts that list on the
//! principal as a Cedar set: `principal.<Type>_<field>` and
//! `principal.actingFor<Type>_<field>`.
//!
//! Each request reads the record through its actor, as the trusted issuer is
//! read, never from the status cache: a cached status is written after the
//! commit and can fall back to an older one once its entry is evicted. So a
//! record moved out of `Active`, or a role taken off it, refuses the next
//! request, with no restart.
//!
//! Cost: one index lookup per marked type, and one actor read for each record
//! the user has. A user with no record pays no read.

use std::collections::BTreeMap;

use temper_runtime::tenant::TenantId;

use crate::identity::error::IdentityError;
use crate::identity::resolver_support::entity_state;
use crate::state::ServerState;

/// The prefix of the attribute naming the status of the user an agent acts for.
const ACTING_FOR: &str = "actingFor";

/// What the records that describe the caller say: `user` is the user calling
/// in person, `acting_for` the user an agent calls for. Keyed by the Cedar
/// principal attribute each value becomes: a record's status as a string,
/// and the items of its `principal_record_set` field as an array (a Cedar set).
pub(super) async fn principal_records(
    state: &ServerState,
    tenant: &TenantId,
    user: Option<&str>,
    acting_for: Option<&str>,
) -> Result<BTreeMap<String, serde_json::Value>, IdentityError> {
    let mut records = BTreeMap::new();
    if user.is_none() && acting_for.is_none() {
        return Ok(records);
    }
    let types = {
        let registry = state
            .registry
            .read()
            .map_err(|_| IdentityError::Unavailable("spec registry lock poisoned".to_string()))?;
        registry
            .principal_record_types(tenant)
            .into_iter()
            .map(|(entity_type, set)| {
                let set = set.map(|field| (field, registry.get_table(tenant, &entity_type)));
                (entity_type, set)
            })
            .collect::<Vec<_>>()
    };
    for (entity_type, set) in types {
        // The index is made complete for the type once per process, so a user
        // with no record is a lookup, not a store read.
        state
            .ensure_type_indexed(tenant, &entity_type)
            .await
            .map_err(|error| {
                IdentityError::Unavailable(format!("{entity_type} index unavailable: {error}"))
            })?;
        let in_person = user.map(|id| (id, entity_type.clone()));
        let for_user = acting_for.map(|id| (id, format!("{ACTING_FOR}{entity_type}")));
        for (id, attribute) in in_person.into_iter().chain(for_user) {
            if !state.entity_exists(tenant, &entity_type, id) {
                continue;
            }
            // A failed read is never an absent record: a forbid on the status
            // would not apply, so the request fails closed instead.
            match entity_state(state, tenant, &entity_type, id).await {
                Ok(Some(record)) => {
                    if let Some((field, table)) = &set {
                        // A record made before the spec declared the field
                        // holds the field's declared initial items.
                        let items = table
                            .as_ref()
                            .and_then(|table| table.initial_values.list(&record.lists, field))
                            .unwrap_or_default();
                        put(&mut records, format!("{attribute}_{field}"), items.into());
                    }
                    put(&mut records, attribute, record.status.into());
                }
                Ok(None) => {}
                Err(error) => {
                    return Err(IdentityError::Unavailable(format!(
                        "{entity_type} read unavailable: {error}"
                    )));
                }
            }
        }
    }
    Ok(records)
}

/// The registry refuses a tenant where two principal-record types would set
/// one attribute (`registry/principal_attributes.rs`), so none is set twice.
fn put(
    records: &mut BTreeMap<String, serde_json::Value>,
    attribute: String,
    value: serde_json::Value,
) {
    let previous = records.insert(attribute, value);
    debug_assert!(previous.is_none(), "a principal attribute set twice");
}
