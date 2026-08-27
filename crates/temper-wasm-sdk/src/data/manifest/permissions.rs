use super::*;

impl ModuleDataGrant {
    /// Whether the grant permits an operation for an exact entity type.
    pub fn permits(
        &self,
        operation: DataOperationKind,
        entity_type: &str,
        action: Option<&str>,
    ) -> bool {
        if !self.operations.contains(&operation) {
            return false;
        }
        let Some(entity) = self
            .entities
            .iter()
            .find(|entity| entity.entity_type == entity_type)
        else {
            return false;
        };
        match operation {
            DataOperationKind::SchemaBundleSubmit
            | DataOperationKind::SchemaBundleGet
            | DataOperationKind::SchemaBundleVerify
            | DataOperationKind::SchemaBundleActivate
            | DataOperationKind::SchemaBundleRetire
            | DataOperationKind::SchemaMigrationStart
            | DataOperationKind::SchemaMigrationGet
            | DataOperationKind::SchemaMigrationRetry
            | DataOperationKind::StreamDescriptorMigrationStart
            | DataOperationKind::StreamDescriptorMigrationAdvance
            | DataOperationKind::StreamDescriptorMigrationGet
            | DataOperationKind::StreamDescriptorMigrationListUnresolved => true,
            DataOperationKind::ActionInvoke => {
                action.is_some_and(|name| entity.actions.contains(name))
            }
            DataOperationKind::CompositeInvoke => {
                action.is_some_and(|name| entity.composite_actions.contains(name))
            }
            DataOperationKind::EntityGet | DataOperationKind::EntityQuery
                if entity.entity_type.rsplit('.').next() == Some("File") =>
            {
                entity
                    .file_operations
                    .contains(&FileOperationKind::MetadataRead)
            }
            _ => true,
        }
    }
}
