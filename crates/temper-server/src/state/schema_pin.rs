//! Durable authority resolution for task-scoped entity schema pins.

use std::collections::BTreeSet;

use temper_runtime::persistence::schema_deployment::{
    SchemaExecutionPin, is_canonical_sha256_digest,
};
use temper_runtime::tenant::TenantId;

use super::ServerState;

pub(crate) const SCHEMA_PIN_MISMATCH_PREFIX: &str = "SchemaPinMismatch:";

fn authoritative_scoped_digest(
    digests: &BTreeSet<String>,
    active_digest: Option<&str>,
) -> Result<Option<String>, &'static str> {
    match digests.len() {
        0 => Ok(None),
        1 => Ok(digests.iter().next().cloned()),
        2 => active_digest
            .filter(|digest| digests.contains(*digest))
            .map(str::to_string)
            .map(Some)
            .ok_or("durable pins do not agree with the committed cutover pointer"),
        _ => Err("more than two durable pins exist"),
    }
}

impl ServerState {
    async fn authoritative_scoped_entity_digest(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        requested_pin: &SchemaExecutionPin,
    ) -> Result<Option<String>, String> {
        let mut digests = BTreeSet::new();
        if let Some((store, _)) = self.event_journal() {
            digests.extend(
                store
                    .scoped_entity_bundle_digests(
                        tenant.as_str(),
                        entity_type,
                        entity_id,
                        &requested_pin.scope,
                        3,
                    )
                    .await
                    .map_err(|error| error.to_string())?
                    .into_iter()
                    .filter(|digest| is_canonical_sha256_digest(digest)),
            );
        }
        let active_digest = if digests.len() == 2 {
            Some(
                self.schema_deployment_store()
                    .ok_or_else(|| {
                        format!(
                            "{SCHEMA_PIN_MISMATCH_PREFIX} scoped entity '{entity_type}/{entity_id}' has ambiguous durable pins and no cutover authority"
                        )
                    })?
                    .active_schema_pointer(tenant.as_str(), &requested_pin.scope)
                    .await
                    .map_err(|error| error.to_string())?
                    .ok_or_else(|| {
                        format!(
                            "{SCHEMA_PIN_MISMATCH_PREFIX} scoped entity '{entity_type}/{entity_id}' has ambiguous durable pins and no active cutover pointer"
                        )
                    })?
                    .bundle_digest,
            )
        } else {
            None
        };
        authoritative_scoped_digest(&digests, active_digest.as_deref()).map_err(|reason| {
            format!(
                "{SCHEMA_PIN_MISMATCH_PREFIX} scoped entity '{entity_type}/{entity_id}' {reason}"
            )
        })
    }

    /// Resolve a scope-only entity request to its durable pin, or the active pin for creation.
    pub(crate) async fn resolve_scope_only_scoped_entity_pin(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        active_pin: SchemaExecutionPin,
    ) -> Result<SchemaExecutionPin, String> {
        let authoritative = self
            .authoritative_scoped_entity_digest(tenant, entity_type, entity_id, &active_pin)
            .await?;
        let resolved = SchemaExecutionPin {
            scope: active_pin.scope,
            bundle_digest: authoritative.unwrap_or(active_pin.bundle_digest),
        };
        let bundle_loaded = self
            .registry
            .read()
            .map_err(|_| "registry lock poisoned".to_string())?
            .get_scoped_config_at_digest(tenant, &resolved.scope, &resolved.bundle_digest)
            .is_some();
        if !bundle_loaded {
            crate::schema_deployment::GovernedSchemaDeploymentService::new(self)
                .recover_registry_bundle(tenant.as_str(), &resolved.scope, &resolved.bundle_digest)
                .await
                .map_err(|error| format!("{SCHEMA_PIN_MISMATCH_PREFIX} {}", error.message()))?;
        }
        Ok(resolved)
    }

    pub(super) async fn scoped_entity_pin_matches(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        requested_pin: &SchemaExecutionPin,
    ) -> Result<bool, String> {
        let authoritative_digest = self
            .authoritative_scoped_entity_digest(tenant, entity_type, entity_id, requested_pin)
            .await?;
        let Some(authoritative_digest) = authoritative_digest else {
            return Ok(false);
        };
        if authoritative_digest == requested_pin.bundle_digest {
            return Ok(true);
        }
        Err(format!(
            "{SCHEMA_PIN_MISMATCH_PREFIX} scoped entity '{entity_type}/{entity_id}' is pinned to {authoritative_digest}, not {}",
            requested_pin.bundle_digest
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn committed_pointer_selects_one_side_of_shadow_cutover() {
        let source = format!("sha256:{}", "a".repeat(64));
        let target = format!("sha256:{}", "b".repeat(64));
        let digests = BTreeSet::from([source.clone(), target.clone()]);

        assert_eq!(
            authoritative_scoped_digest(&digests, Some(&source)),
            Ok(Some(source))
        );
        assert_eq!(
            authoritative_scoped_digest(&digests, Some(&target)),
            Ok(Some(target))
        );
        assert!(authoritative_scoped_digest(&digests, None).is_err());
    }

    #[test]
    fn single_durable_pin_is_not_reinterpreted_by_pointer_change() {
        let source = format!("sha256:{}", "a".repeat(64));
        let replacement = format!("sha256:{}", "b".repeat(64));
        let digests = BTreeSet::from([source.clone()]);

        assert_eq!(
            authoritative_scoped_digest(&digests, Some(&replacement)),
            Ok(Some(source))
        );
    }
}
