//! Invocation capability and Cedar authorization checks.

use std::collections::BTreeMap;

use temper_wasm_sdk::data::{
    DataOperationKind, FileOperationKind, ModuleDataError, ModuleDataErrorKind,
};

use super::{ApplicationDataInvocation, GovernedApplicationDataService, data_error, short_type};

impl ApplicationDataInvocation {
    pub(super) fn require(
        &self,
        kind: DataOperationKind,
        entity_type: &str,
        action: Option<&str>,
    ) -> Result<(), ModuleDataError> {
        if self
            .authority
            .binding
            .grant
            .permits(kind, entity_type, action)
        {
            return Ok(());
        }
        Err(data_error(
            ModuleDataErrorKind::AuthorizationDenied,
            "CapabilityDenied",
            "module data grant does not permit this operation",
        ))
    }

    pub(super) fn require_file(
        &self,
        kind: DataOperationKind,
        file_operation: FileOperationKind,
    ) -> Result<String, ModuleDataError> {
        if !self.authority.binding.grant.operations.contains(&kind) {
            return Err(data_error(
                ModuleDataErrorKind::AuthorizationDenied,
                "CapabilityDenied",
                "module data grant does not permit this File operation",
            ));
        }
        self.authority
            .binding
            .grant
            .entities
            .iter()
            .find(|entity| {
                short_type(&entity.entity_type) == "File"
                    && entity.file_operations.contains(&file_operation)
            })
            .map(|entity| entity.entity_type.clone())
            .ok_or_else(|| {
                data_error(
                    ModuleDataErrorKind::AuthorizationDenied,
                    "FileCapabilityDenied",
                    "module data grant does not permit this File capability",
                )
            })
    }

    pub(super) fn authorize(
        &self,
        action: &str,
        entity_type: &str,
        entity_id: Option<&str>,
    ) -> Result<(), ModuleDataError> {
        self.authorize_value(action, entity_type, entity_id, None)
    }

    pub(super) fn authorize_value(
        &self,
        action: &str,
        entity_type: &str,
        entity_id: Option<&str>,
        value: Option<&serde_json::Map<String, serde_json::Value>>,
    ) -> Result<(), ModuleDataError> {
        let mut attrs = BTreeMap::new();
        if let Some(entity_id) = entity_id {
            attrs.insert("id".into(), serde_json::Value::String(entity_id.into()));
        }
        attrs.insert(
            "module_name".into(),
            self.authority.module_name.clone().into(),
        );
        attrs.insert(
            "module_artifact".into(),
            self.authority.artifact_digest.clone().into(),
        );
        attrs.insert(
            "module_trigger".into(),
            self.authority.trigger.clone().into(),
        );
        attrs.insert(
            "module_trigger_entity_type".into(),
            self.authority.triggering_entity_type.clone().into(),
        );
        attrs.insert(
            "module_grant_digest".into(),
            self.authority.grant_digest.clone().into(),
        );
        if let Some(value) = value {
            attrs.extend(value.clone());
            let status = value
                .get("Status")
                .or_else(|| value.get("status"))
                .cloned()
                .unwrap_or_else(|| serde_json::Value::String(String::new()));
            attrs.insert("status".into(), status);
        }
        attrs.insert(
            "has_spec".into(),
            self.state
                .has_registered_spec(&self.authority.tenant, short_type(entity_type))
                .unwrap_or(false)
                .into(),
        );
        GovernedApplicationDataService::new(&self.state)
            .authorize(
                &self.authority.tenant,
                &self.authority.security,
                action,
                short_type(entity_type),
                &attrs,
            )
            .map_err(|_| {
                data_error(
                    ModuleDataErrorKind::AuthorizationDenied,
                    "AuthorizationDenied",
                    "caller is not authorized for this operation",
                )
            })
    }
}
