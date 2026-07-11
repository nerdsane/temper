//! Exact source manifests for durable registry compare-and-set operations.

use std::collections::{BTreeMap, BTreeSet};

use super::{SpecRow, TenantConstraintRow};

/// Exact committed source set validated by one registry restore attempt.
///
/// `spec_versions` is the complete set in scope, not only quarantined rows.
/// `constraint_versions` contains one entry for every tenant represented by a
/// spec and distinguishes an absent constraint row (`None`) from a versioned
/// row (`Some`). Storage adapters compare this manifest inside the same
/// transaction that mutates quarantine state so insertions and removals cannot
/// escape the compare-and-set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegistrySourceSnapshot {
    /// Complete `(tenant, entity_type) -> committed version` map in scope.
    pub spec_versions: BTreeMap<(String, String), i64>,
    /// Exact constraint presence/version for every tenant in `spec_versions`.
    pub constraint_versions: BTreeMap<String, Option<i64>>,
}

impl RegistrySourceSnapshot {
    /// Build an exact manifest from backend-neutral rows.
    pub fn from_rows(
        specs: &[SpecRow],
        constraints: &[TenantConstraintRow],
    ) -> Result<Self, String> {
        let mut spec_versions = BTreeMap::new();
        let mut tenants = BTreeSet::new();
        for row in specs {
            if !row.committed || row.version <= 0 {
                return Err(
                    "registry source snapshot requires positive committed specs".to_string()
                );
            }
            let key = (row.tenant.clone(), row.entity_type.clone());
            if spec_versions.insert(key, row.version).is_some() {
                return Err(format!(
                    "registry source snapshot contains duplicate entity '{}:{}'",
                    row.tenant, row.entity_type
                ));
            }
            tenants.insert(row.tenant.clone());
        }

        let mut persisted_constraints = BTreeMap::new();
        for row in constraints {
            if row.version <= 0 {
                return Err("registry constraint snapshot version must be positive".to_string());
            }
            if persisted_constraints
                .insert(row.tenant.clone(), row.version)
                .is_some()
            {
                return Err(format!(
                    "registry source snapshot contains duplicate constraints for tenant '{}'",
                    row.tenant
                ));
            }
        }
        let constraint_versions = tenants
            .into_iter()
            .map(|tenant| {
                let version = persisted_constraints.get(&tenant).copied();
                (tenant, version)
            })
            .collect();
        Ok(Self {
            spec_versions,
            constraint_versions,
        })
    }

    /// Return the manifest for exactly one tenant.
    pub fn for_tenant(&self, tenant: &str) -> Self {
        Self {
            spec_versions: self
                .spec_versions
                .iter()
                .filter(|((row_tenant, _), _)| row_tenant == tenant)
                .map(|(key, version)| (key.clone(), *version))
                .collect(),
            constraint_versions: self
                .constraint_versions
                .get(tenant)
                .map(|version| BTreeMap::from([(tenant.to_string(), *version)]))
                .unwrap_or_default(),
        }
    }

    /// Return the tenants represented by committed specs.
    pub fn tenants(&self) -> BTreeSet<&str> {
        self.spec_versions
            .keys()
            .map(|(tenant, _)| tenant.as_str())
            .collect()
    }
}
