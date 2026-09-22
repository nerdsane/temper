//! Restore admission status when an unchanged spec skips background verification.

use std::collections::{BTreeMap, HashMap};

use temper_runtime::tenant::TenantId;
use temper_server::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};

pub(super) fn ioa_sources_requiring_background_verification(
    ioa_sources: HashMap<String, String>,
    verification_cache: &BTreeMap<String, (String, bool)>,
    registry: &mut SpecRegistry,
    tenant: &TenantId,
) -> HashMap<String, String> {
    ioa_sources
        .into_iter()
        .filter(|(entity_name, ioa_source)| {
            let hash = temper_store_turso::spec_content_hash(ioa_source);
            let cached = verification_cache
                .get(entity_name)
                .is_some_and(|(cached_hash, verified)| *verified && cached_hash == &hash);
            let registered = registry.get_spec(tenant, entity_name).is_some_and(|spec| {
                temper_store_turso::spec_content_hash(&spec.ioa_source) == hash
            });
            if !cached || !registered {
                return true;
            }
            if !registry
                .get_verification_status(tenant, entity_name)
                .is_some_and(VerificationStatus::is_passed)
            {
                registry.set_verification_status(
                    tenant,
                    entity_name,
                    VerificationStatus::Restored(EntityVerificationResult {
                        all_passed: true,
                        levels: vec![EntityLevelSummary {
                            level: "Persisted".into(),
                            passed: true,
                            summary: "Restored passed verification for the unchanged spec hash"
                                .into(),
                            details: None,
                        }],
                        // The compact cache does not contain the original verification time.
                        verified_at: String::new(),
                    }),
                );
            }
            false
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use temper_spec::csdl::parse_csdl;

    #[test]
    fn cached_verification_restores_admission_only_for_identical_passed_specs() {
        let tenant = TenantId::default();
        let csdl = r#"<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
<edmx:DataServices><Schema Namespace="Demo" xmlns="http://docs.oasis-open.org/odata/ns/edm"/></edmx:DataServices></edmx:Edmx>"#;
        let sources: HashMap<String, String> = [
            "Unchanged",
            "Changed",
            "Failed",
            "RegistryChanged",
        ]
        .into_iter()
        .map(|name| {
            (
                name.into(),
                format!(
                    "[automaton]\nname = \"{name}\"\nstates = [\"Open\"]\ninitial = \"Open\"\n"
                ),
            )
        })
        .collect();
        let mut registry = SpecRegistry::new();
        let registered: Vec<(&str, &str)> = sources
            .iter()
            .map(|(name, source)| (name.as_str(), source.as_str()))
            .collect();
        registry.register_tenant(
            tenant.clone(),
            parse_csdl(csdl).unwrap(),
            csdl.into(),
            &registered,
        );
        assert!(
            !registry
                .get_verification_status(&tenant, "Unchanged")
                .unwrap()
                .is_passed()
        );
        let cache = sources
            .iter()
            .map(|(name, source)| {
                let hash = temper_store_turso::spec_content_hash(if name == "Changed" {
                    "old source"
                } else {
                    source
                });
                (name.clone(), (hash, name != "Failed"))
            })
            .collect();
        let mut loaded = sources;
        loaded
            .get_mut("RegistryChanged")
            .unwrap()
            .push_str("\n# new disk source\n");
        let pending =
            ioa_sources_requiring_background_verification(loaded, &cache, &mut registry, &tenant);
        assert_eq!(pending.len(), 3);
        assert!(!pending.contains_key("Unchanged"));
        assert!(
            matches!(registry.get_verification_status(&tenant, "Unchanged"), Some(VerificationStatus::Restored(result)) if result.all_passed)
        );
        for name in ["Changed", "Failed", "RegistryChanged"] {
            assert!(pending.contains_key(name));
            assert!(
                !registry
                    .get_verification_status(&tenant, name)
                    .unwrap()
                    .is_passed()
            );
        }
    }
}
