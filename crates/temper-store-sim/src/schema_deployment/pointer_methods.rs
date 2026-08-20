macro_rules! impl_schema_pointer_method {
    () => {
        async fn active_schema_pointer(
            &self,
            tenant: &str,
            scope: &SchemaScope,
        ) -> Result<Option<SchemaActivePointer>, SchemaDeploymentStoreError> {
            let inner = self.inner.lock().map_err(|_| {
                SchemaDeploymentStoreError::BackendUnavailable("lock poisoned".into())
            })?;
            Ok(inner
                .schema_deployments
                .active
                .get(&(tenant.to_string(), scope.clone()))
                .cloned())
        }
    };
}
