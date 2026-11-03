//! The Cedar attributes a tenant's principal-record types put on the
//! principal, and the refusal of a tenant where two of them share one.
//!
//! A `principal_record` type `T` sets `principal.T` (its status) and, with a
//! `principal_record_set` field `f`, `principal.T_f` (the set); an agent's user
//! gets the same names behind `actingFor`. Type `A` with field `b_c` and type
//! `A_b` with field `c` would both set `principal.A_b_c`, and a type named
//! `Role_roles` would set the attribute that type `Role`'s `roles` set sets.
//! Either value could then be the one a policy reads, so the registry refuses
//! such a tenant at install, naming both types, and a request never meets it.
//!
//! The `actingFor` forms clash exactly when the in-person forms do: they add
//! one prefix to both, and an in-person name starts uppercase while an
//! `actingFor` name starts lowercase, so the in-person names are checked.

use std::collections::BTreeMap;

use temper_runtime::tenant::TenantId;
use temper_spec::automaton::{AutomatonMeta, parse_automaton};

use super::{SpecRegistry, types::RegistryError};

/// Refuse `types`, every type the tenant would hold after an install, when
/// two principal-record types reach one principal attribute.
fn refuse_shared_principal_attributes<'a>(
    tenant: &str,
    types: impl IntoIterator<Item = (&'a str, &'a AutomatonMeta)>,
) -> Result<(), RegistryError> {
    let mut owners: BTreeMap<String, &str> = BTreeMap::new();
    for (entity_type, meta) in types {
        if !meta.principal_record {
            continue;
        }
        let set = meta
            .principal_record_set
            .as_ref()
            .map(|field| format!("{entity_type}_{field}"));
        for attribute in std::iter::once(entity_type.to_string()).chain(set) {
            if let Some(first) = owners.insert(attribute.clone(), entity_type) {
                return Err(RegistryError::PrincipalAttributeShared {
                    tenant: tenant.to_string(),
                    first: first.to_string(),
                    second: entity_type.to_string(),
                    attribute,
                });
            }
        }
    }
    Ok(())
}

impl SpecRegistry {
    /// Check the principal attributes of an install before persistent writes.
    /// Registration repeats this check against the registry at the swap.
    pub fn check_principal_attributes(
        &self,
        tenant: &TenantId,
        sources: &[(&str, &str)],
        merge: bool,
    ) -> Result<(), RegistryError> {
        let parsed = sources
            .iter()
            .map(|(name, source)| {
                parse_automaton(source)
                    .map(|automaton| (*name, automaton.automaton))
                    .map_err(|error| RegistryError::IoaParse {
                        tenant: tenant.to_string(),
                        entity_type: (*name).to_string(),
                        source: error.to_string(),
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.check_principal_attribute_metas(
            tenant,
            parsed.iter().map(|(name, meta)| (*name, meta)),
            merge,
        )
    }

    /// Check the types the tenant will hold: a merge keeps types it does not
    /// replace, and a later spec of the same registered type wins.
    pub(super) fn check_principal_attribute_metas<'a>(
        &'a self,
        tenant: &TenantId,
        incoming: impl IntoIterator<Item = (&'a str, &'a AutomatonMeta)>,
        merge: bool,
    ) -> Result<(), RegistryError> {
        let kept = self
            .tenants
            .get(tenant)
            .filter(|_| merge)
            .into_iter()
            .flat_map(|config| &config.entities)
            .map(|(name, spec)| (name.as_str(), &spec.automaton.automaton));
        let after: BTreeMap<_, _> = kept.chain(incoming).collect();
        refuse_shared_principal_attributes(tenant.as_str(), after)
    }
}
