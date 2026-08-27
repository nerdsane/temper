macro_rules! impl_scoped_event_store_methods {
    () => {
        /// Page durable entity IDs for one immutable scoped-schema journal set.
        fn list_scoped_entity_ids_page(
            &self,
            tenant: &str,
            entity_type: &str,
            scope: &schema_deployment::SchemaScope,
            bundle_digest: &str,
            after_entity_id: Option<&str>,
            limit: usize,
        ) -> impl std::future::Future<Output = Result<Vec<String>, PersistenceError>> + Send {
            async move {
                if limit == 0 {
                    return Ok(Vec::new());
                }
                const JOURNAL_PAGE_BUDGET: usize = 256;
                let suffix = schema_deployment::scoped_journal_pin_suffix(
                    &schema_deployment::SchemaExecutionPin {
                        scope: scope.clone(),
                        bundle_digest: bundle_digest.to_string(),
                    },
                );
                let mut cursor: Option<(String, String)> = None;
                let mut entity_ids = Vec::new();
                while entity_ids.len() < limit {
                    let journals = self
                        .list_journal_ids_page(
                            tenant,
                            Some(entity_type),
                            cursor
                                .as_ref()
                                .map(|(found_type, id)| (found_type.as_str(), id.as_str())),
                            JOURNAL_PAGE_BUDGET,
                        )
                        .await?;
                    let page_len = journals.len();
                    let Some(last) = journals.last().cloned() else {
                        break;
                    };
                    cursor = Some(last);
                    entity_ids.extend(journals.into_iter().filter_map(|(_, journal_entity_id)| {
                        journal_entity_id
                            .strip_suffix(&suffix)
                            .filter(|entity_id| {
                                after_entity_id.is_none_or(|after| *entity_id > after)
                            })
                            .map(str::to_string)
                    }));
                    if page_len < JOURNAL_PAGE_BUDGET {
                        break;
                    }
                }
                entity_ids.sort();
                entity_ids.truncate(limit);
                Ok(entity_ids)
            }
        }

        /// Return bounded durable bundle digests for one scoped entity identity.
        fn scoped_entity_bundle_digests(
            &self,
            _tenant: &str,
            _entity_type: &str,
            _entity_id: &str,
            _scope: &schema_deployment::SchemaScope,
            _limit: usize,
        ) -> impl std::future::Future<Output = Result<Vec<String>, PersistenceError>> + Send {
            async {
                Err(PersistenceError::Storage(
                    "scoped entity pin lookup is unsupported by this event store".to_string(),
                ))
            }
        }
    };
}
