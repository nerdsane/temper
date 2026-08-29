macro_rules! dyn_stream_publication_declarations {
    () => {
        /// List one bounded page of legacy unscoped entity journals.
        fn list_unscoped_entity_ids_page<'a>(
            &'a self,
            tenant: &'a str,
            entity_type: &'a str,
            after_entity_id: Option<&'a str>,
            limit: usize,
        ) -> EventStoreFuture<'a, Result<Vec<String>, PersistenceError>>;

        /// Read the monotonic publication generation for one unscoped entity type.
        fn unscoped_entity_type_write_version<'a>(
            &'a self,
            tenant: &'a str,
            entity_type: &'a str,
        ) -> EventStoreFuture<'a, Result<u64, PersistenceError>>;

        /// Atomically activate an installed-application stream publication fence.
        fn activate_unscoped_stream_publication_fence<'a>(
            &'a self,
            tenant: &'a str,
            fence: &'a temper_runtime::persistence::schema_deployment::StreamPublicationFence,
        ) -> EventStoreFuture<'a, Result<(), PersistenceError>>;

        /// Deactivate an exact installed-application publication fence.
        fn deactivate_unscoped_stream_publication_fence<'a>(
            &'a self,
            tenant: &'a str,
            application_id: &'a str,
            semantic_digest: Option<&'a str>,
        ) -> EventStoreFuture<'a, Result<(), PersistenceError>>;

        /// Read the active publication fence owned by an installed application.
        fn get_unscoped_stream_publication_fence<'a>(
            &'a self,
            tenant: &'a str,
            application_id: &'a str,
        ) -> EventStoreFuture<
            'a,
            Result<
                Option<temper_runtime::persistence::schema_deployment::StreamPublicationFence>,
                PersistenceError,
            >,
        >;

        /// Test whether an exact entity capability fence is durably active.
        fn unscoped_stream_publication_fence_active<'a>(
            &'a self,
            tenant: &'a str,
            entity_type: &'a str,
            publication_action: &'a str,
            capability_digest: &'a str,
        ) -> EventStoreFuture<'a, Result<bool, PersistenceError>>;

        /// Restore a prior installed-application publication fence atomically.
        fn restore_unscoped_stream_publication_fence<'a>(
            &'a self,
            tenant: &'a str,
            expected_current_semantic_digest: &'a str,
            fence: &'a temper_runtime::persistence::schema_deployment::StreamPublicationFence,
        ) -> EventStoreFuture<'a, Result<(), PersistenceError>>;
    };
}

macro_rules! dyn_stream_publication_impl {
    () => {
        fn list_unscoped_entity_ids_page<'a>(
            &'a self,
            tenant: &'a str,
            entity_type: &'a str,
            after_entity_id: Option<&'a str>,
            limit: usize,
        ) -> EventStoreFuture<'a, Result<Vec<String>, PersistenceError>> {
            Box::pin(EventStore::list_unscoped_entity_ids_page(
                self,
                tenant,
                entity_type,
                after_entity_id,
                limit,
            ))
        }

        fn unscoped_entity_type_write_version<'a>(
            &'a self,
            tenant: &'a str,
            entity_type: &'a str,
        ) -> EventStoreFuture<'a, Result<u64, PersistenceError>> {
            Box::pin(EventStore::unscoped_entity_type_write_version(
                self,
                tenant,
                entity_type,
            ))
        }

        fn activate_unscoped_stream_publication_fence<'a>(
            &'a self,
            tenant: &'a str,
            fence: &'a temper_runtime::persistence::schema_deployment::StreamPublicationFence,
        ) -> EventStoreFuture<'a, Result<(), PersistenceError>> {
            Box::pin(EventStore::activate_unscoped_stream_publication_fence(
                self, tenant, fence,
            ))
        }

        fn deactivate_unscoped_stream_publication_fence<'a>(
            &'a self,
            tenant: &'a str,
            application_id: &'a str,
            semantic_digest: Option<&'a str>,
        ) -> EventStoreFuture<'a, Result<(), PersistenceError>> {
            Box::pin(EventStore::deactivate_unscoped_stream_publication_fence(
                self,
                tenant,
                application_id,
                semantic_digest,
            ))
        }

        fn get_unscoped_stream_publication_fence<'a>(
            &'a self,
            tenant: &'a str,
            application_id: &'a str,
        ) -> EventStoreFuture<
            'a,
            Result<
                Option<temper_runtime::persistence::schema_deployment::StreamPublicationFence>,
                PersistenceError,
            >,
        > {
            Box::pin(EventStore::get_unscoped_stream_publication_fence(
                self,
                tenant,
                application_id,
            ))
        }

        fn unscoped_stream_publication_fence_active<'a>(
            &'a self,
            tenant: &'a str,
            entity_type: &'a str,
            publication_action: &'a str,
            capability_digest: &'a str,
        ) -> EventStoreFuture<'a, Result<bool, PersistenceError>> {
            Box::pin(EventStore::unscoped_stream_publication_fence_active(
                self,
                tenant,
                entity_type,
                publication_action,
                capability_digest,
            ))
        }

        fn restore_unscoped_stream_publication_fence<'a>(
            &'a self,
            tenant: &'a str,
            expected_current_semantic_digest: &'a str,
            fence: &'a temper_runtime::persistence::schema_deployment::StreamPublicationFence,
        ) -> EventStoreFuture<'a, Result<(), PersistenceError>> {
            Box::pin(EventStore::restore_unscoped_stream_publication_fence(
                self,
                tenant,
                expected_current_semantic_digest,
                fence,
            ))
        }
    };
}

macro_rules! boxed_stream_publication_methods {
    () => {
        /// List one bounded page of legacy unscoped entity journals.
        pub async fn list_unscoped_entity_ids_page(
            &self,
            tenant: &str,
            entity_type: &str,
            after_entity_id: Option<&str>,
            limit: usize,
        ) -> Result<Vec<String>, PersistenceError> {
            self.0
                .list_unscoped_entity_ids_page(tenant, entity_type, after_entity_id, limit)
                .await
        }

        /// Read the monotonic publication generation for one unscoped entity type.
        pub async fn unscoped_entity_type_write_version(
            &self,
            tenant: &str,
            entity_type: &str,
        ) -> Result<u64, PersistenceError> {
            self.0
                .unscoped_entity_type_write_version(tenant, entity_type)
                .await
        }

        /// Atomically activate an installed-application stream publication fence.
        pub async fn activate_unscoped_stream_publication_fence(
            &self,
            tenant: &str,
            fence: &temper_runtime::persistence::schema_deployment::StreamPublicationFence,
        ) -> Result<(), PersistenceError> {
            self.0
                .activate_unscoped_stream_publication_fence(tenant, fence)
                .await
        }

        /// Deactivate an exact installed-application publication fence.
        pub async fn deactivate_unscoped_stream_publication_fence(
            &self,
            tenant: &str,
            application_id: &str,
            semantic_digest: Option<&str>,
        ) -> Result<(), PersistenceError> {
            self.0
                .deactivate_unscoped_stream_publication_fence(
                    tenant,
                    application_id,
                    semantic_digest,
                )
                .await
        }

        /// Read the active publication fence owned by an installed application.
        pub async fn get_unscoped_stream_publication_fence(
            &self,
            tenant: &str,
            application_id: &str,
        ) -> Result<
            Option<temper_runtime::persistence::schema_deployment::StreamPublicationFence>,
            PersistenceError,
        > {
            self.0
                .get_unscoped_stream_publication_fence(tenant, application_id)
                .await
        }

        /// Test whether an exact entity capability fence is durably active.
        pub async fn unscoped_stream_publication_fence_active(
            &self,
            tenant: &str,
            entity_type: &str,
            publication_action: &str,
            capability_digest: &str,
        ) -> Result<bool, PersistenceError> {
            self.0
                .unscoped_stream_publication_fence_active(
                    tenant,
                    entity_type,
                    publication_action,
                    capability_digest,
                )
                .await
        }

        /// Restore a prior installed-application publication fence atomically.
        pub async fn restore_unscoped_stream_publication_fence(
            &self,
            tenant: &str,
            expected_current_semantic_digest: &str,
            fence: &temper_runtime::persistence::schema_deployment::StreamPublicationFence,
        ) -> Result<(), PersistenceError> {
            self.0
                .restore_unscoped_stream_publication_fence(
                    tenant,
                    expected_current_semantic_digest,
                    fence,
                )
                .await
        }
    };
}
