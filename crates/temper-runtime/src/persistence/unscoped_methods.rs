macro_rules! impl_unscoped_event_store_methods {
    () => {
        /// Page tenant-global journals, excluding canonical task-scoped journals.
        fn list_unscoped_entity_ids_page(
            &self,
            tenant: &str,
            entity_type: &str,
            after_entity_id: Option<&str>,
            limit: usize,
        ) -> impl std::future::Future<Output = Result<Vec<String>, PersistenceError>> + Send {
            async move {
                let rows = self
                    .list_journal_ids_page(
                        tenant,
                        Some(entity_type),
                        after_entity_id.map(|after| (entity_type, after)),
                        limit,
                    )
                    .await?;
                Ok(rows
                    .into_iter()
                    .filter_map(|(_, entity_id)| {
                        (!schema_deployment::is_reserved_scoped_journal_entity_id(&entity_id))
                            .then_some(entity_id)
                    })
                    .collect())
            }
        }

        /// Return the monotonic event count for one tenant-global entity type.
        fn unscoped_entity_type_write_version(
            &self,
            tenant: &str,
            entity_type: &str,
        ) -> impl std::future::Future<Output = Result<u64, PersistenceError>> + Send {
            async move {
                let ids = self
                    .list_unscoped_entity_ids_page(tenant, entity_type, None, usize::MAX)
                    .await?;
                let mut total = 0_u64;
                for entity_id in ids {
                    let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
                    let count = self.read_events(&persistence_id, 0).await?.len();
                    total = total
                        .checked_add(u64::try_from(count).map_err(|_| {
                            PersistenceError::Storage("global write version exhausted".into())
                        })?)
                        .ok_or_else(|| {
                            PersistenceError::Storage("global write version exhausted".into())
                        })?;
                }
                Ok(total)
            }
        }

        /// Atomically validate and install one tenant-global stream publication fence.
        fn activate_unscoped_stream_publication_fence(
            &self,
            tenant: &str,
            fence: &schema_deployment::StreamPublicationFence,
        ) -> impl std::future::Future<Output = Result<(), PersistenceError>> + Send {
            let _ = (tenant, fence);
            async {
                Err(PersistenceError::Storage(
                    "unscoped stream publication fencing is unsupported".into(),
                ))
            }
        }

        /// Remove one exact tenant-global application publication fence.
        fn deactivate_unscoped_stream_publication_fence(
            &self,
            tenant: &str,
            application_id: &str,
            semantic_digest: Option<&str>,
        ) -> impl std::future::Future<Output = Result<(), PersistenceError>> + Send {
            let _ = (tenant, application_id, semantic_digest);
            async { Ok(()) }
        }

        /// Read one installed application's current publication fence.
        fn get_unscoped_stream_publication_fence(
            &self,
            tenant: &str,
            application_id: &str,
        ) -> impl std::future::Future<
            Output = Result<Option<schema_deployment::StreamPublicationFence>, PersistenceError>,
        > + Send {
            let _ = (tenant, application_id);
            async { Ok(None) }
        }

        /// Report whether strict stream publication is durably active for one type.
        fn unscoped_stream_publication_fence_active(
            &self,
            tenant: &str,
            entity_type: &str,
            publication_action: &str,
            capability_digest: &str,
        ) -> impl std::future::Future<Output = Result<bool, PersistenceError>> + Send {
            let _ = (tenant, entity_type, publication_action, capability_digest);
            async { Ok(false) }
        }

        /// Atomically restore a previously active application fence during rollback.
        fn restore_unscoped_stream_publication_fence(
            &self,
            tenant: &str,
            expected_current_semantic_digest: &str,
            fence: &schema_deployment::StreamPublicationFence,
        ) -> impl std::future::Future<Output = Result<(), PersistenceError>> + Send {
            let _ = (tenant, expected_current_semantic_digest, fence);
            async {
                Err(PersistenceError::Storage(
                    "unscoped stream publication fence restore is unsupported".into(),
                ))
            }
        }
    };
}
