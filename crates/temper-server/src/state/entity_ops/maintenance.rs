//! Passivation, metadata enrichment, and verification gates.

use super::*;

impl ServerState {
    /// Passivate actors that have been idle longer than the configured timeout.
    ///
    /// Keeps `entity_index` entries intact so future accesses can lazy-spawn.
    #[instrument(skip_all, fields(otel.name = "entity.passivate_idle_actors"))]
    pub async fn passivate_idle_actors(&self) {
        let timeout_secs = actor_idle_timeout_secs();
        let cutoff = sim_now() - chrono::Duration::seconds(timeout_secs);

        let candidates: Vec<(String, ActorRef<EntityMsg>)> = {
            let Ok(registry) = self.actor_registry.read() else {
                return;
            };
            let Ok(last_accessed) = self.last_accessed.read() else {
                return;
            };
            registry
                .iter()
                .filter_map(|(key, actor_ref)| {
                    let last_seen = last_accessed.get(key)?;
                    if *last_seen <= cutoff {
                        Some((key.clone(), actor_ref.clone()))
                    } else {
                        None
                    }
                })
                .collect()
        };

        if candidates.is_empty() {
            return;
        }

        let mut passivated = 0usize;
        let policy = self.dispatch_retry_policy();
        let journal = self.event_journal();
        for (actor_key, actor_ref) in candidates {
            // ADR-0048: retry transient failures so passivation is not skipped
            // by a single AskTimeout under load.
            let snapshot_outcome = retry::ask_with_backoff::<_, EntityResponse, _>(
                &actor_ref,
                || EntityMsg::GetState,
                &policy,
            )
            .await;
            if let Some((store, _backend)) = journal.as_ref()
                && let Ok(response) = &snapshot_outcome.result
                && response.state.sequence_nr > 0
            {
                let snapshot_bytes = match EntityActor::serialize_snapshot_state(&response.state) {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        tracing::warn!(actor_key = %actor_key, error = %e, "failed to encode passivation snapshot");
                        Vec::new()
                    }
                };
                if !snapshot_bytes.is_empty()
                    && let Err(e) = store
                        .save_snapshot(&actor_key, response.state.sequence_nr, &snapshot_bytes)
                        .await
                {
                    tracing::warn!(
                        actor_key = %actor_key,
                        seq = response.state.sequence_nr,
                        error = %e,
                        "failed to save snapshot during passivation"
                    );
                }
            }

            // Candidate selection and snapshot persistence both yield. Do not
            // passivate an actor that was touched while either was in flight.
            let still_idle_current = {
                let Ok(registry) = self.actor_registry.read() else {
                    tracing::error!(
                        actor_key = %actor_key,
                        "actor registry lock poisoned while rechecking passivation"
                    );
                    continue;
                };
                let Ok(last_accessed) = self.last_accessed.read() else {
                    tracing::error!(
                        actor_key = %actor_key,
                        "last-accessed lock poisoned while rechecking passivation"
                    );
                    continue;
                };
                registry.get(&actor_key).is_some_and(|current| {
                    !current.is_stopped() && current.id().uid == actor_ref.id().uid
                }) && last_accessed
                    .get(&actor_key)
                    .is_some_and(|last_seen| *last_seen <= cutoff)
            };
            if !still_idle_current {
                continue;
            }

            let Some((tenant_name, remainder)) = actor_key.split_once(':') else {
                tracing::error!(actor_key = %actor_key, "invalid actor key during passivation");
                continue;
            };
            let Some((entity_type, entity_id)) = remainder.split_once(':') else {
                tracing::error!(actor_key = %actor_key, "invalid actor key during passivation");
                continue;
            };
            let tenant = TenantId::from(tenant_name.to_string());
            let Some((store, backend)) = journal.as_ref() else {
                // Preserve the bounded in-memory fallback: it has no durable
                // tail to replay, but its historical contract passivates idle
                // actors and recreates them from their initial state. Drain
                // admitted work first so the compatibility path never drops a
                // mailbox message merely because the idle scan raced it.
                let drain_guard = match actor_ref.stop_and_wait().await {
                    Ok(drain_guard) => drain_guard,
                    Err(error) => {
                        tracing::error!(
                            actor_key = %actor_key,
                            error = %error,
                            "failed to drain memory-only actor during passivation"
                        );
                        continue;
                    }
                };
                let removed = self.remove_entity_actor_incarnation_if_current(
                    &tenant,
                    entity_type,
                    entity_id,
                    Some(actor_ref.id().uid),
                    false,
                );
                drop(drain_guard);
                if removed {
                    if let Ok(mut cache) = self.entity_state_cache.lock() {
                        cache.pop(&actor_key);
                    }
                    passivated += 1;
                }
                continue;
            };
            let Some(table) = projection_backfill::transition_table_for(self, &tenant, entity_type)
            else {
                tracing::error!(
                    actor_key = %actor_key,
                    "skipping passivation because no transition table can replay the durable tail"
                );
                continue;
            };
            let tenant_blob_store = self.blob_store_for_tenant(&tenant).ok();

            // Keep this incarnation registry-visible until its FIFO mailbox is
            // closed. Requests admitted before Stop either complete first or
            // are reflected in the authoritative replay below; a replacement
            // cannot be published from a stale pre-drain snapshot.
            let drain_guard = match actor_ref.stop_and_wait().await {
                Ok(drain_guard) => drain_guard,
                Err(error) => {
                    tracing::error!(
                        actor_key = %actor_key,
                        error = %error,
                        "failed to drain actor during passivation"
                    );
                    continue;
                }
            };

            let recovered = match recover_entity_state_from_store(
                tenant.as_str(),
                entity_type,
                entity_id,
                &table,
                store,
                *backend,
                &serde_json::json!({}),
                tenant_blob_store.as_ref(),
                true,
            )
            .await
            {
                Ok(recovered) => recovered,
                Err(error) => {
                    tracing::error!(
                        actor_key = %actor_key,
                        error = %error,
                        "failed to reconcile the durable tail after passivation drain"
                    );
                    let _ = self.remove_entity_actor_incarnation_if_current(
                        &tenant,
                        entity_type,
                        entity_id,
                        Some(actor_ref.id().uid),
                        true,
                    );
                    drop(drain_guard);
                    if !self
                        .ensure_entity_actor_materialized(&tenant, entity_type, entity_id)
                        .await
                    {
                        tracing::error!(
                            actor_key = %actor_key,
                            "failed to replace actor after passivation reconciliation failure"
                        );
                    }
                    continue;
                }
            };
            let inactive_timeout_fence = self.reconcile_state_timeout_after_synthetic_commit(
                &tenant,
                entity_type,
                entity_id,
                &recovered,
            );

            let removed = self.remove_entity_actor_incarnation_if_current(
                &tenant,
                entity_type,
                entity_id,
                Some(actor_ref.id().uid),
                recovered.status == "Deleted",
            );
            drop(drain_guard);

            if removed {
                // Evict the state cache entry so stale status doesn't linger.
                if let Ok(mut cache) = self.entity_state_cache.lock() {
                    cache.pop(&actor_key);
                }
                self.release_inactive_state_timeout_after_actor_eviction(
                    &tenant,
                    entity_type,
                    entity_id,
                    inactive_timeout_fence,
                );
                passivated += 1;
            }
        }

        if passivated > 0 {
            runtime_metrics::record_server_state_metrics(self);
            tracing::info!(count = passivated, timeout_secs, "passivated idle actors");
        }
    }

    /// Update Agent.Hint annotations based on trajectory analysis.
    pub fn enrich_metadata(&self, action_name: &str, hint: &str) {
        const AGENT_HINTS_BUDGET: usize = 1_000;
        let Ok(mut hints) = self.agent_hints.write() else {
            return;
        };
        hints.insert(action_name.to_string(), hint.to_string());
        while hints.len() > AGENT_HINTS_BUDGET {
            let oldest_key = hints.iter().next().map(|(k, _)| k.clone());
            if let Some(k) = oldest_key {
                hints.remove(&k);
            } else {
                break;
            }
        }
    }

    /// Check the verification gate for a specific entity type.
    ///
    /// Returns `Ok(())` if the entity type is verified and operations are allowed.
    /// Returns `Err(VerificationGateError)` if operations should be blocked.
    ///
    /// Policy:
    /// - `None` → `Ok(())` (backward compat for legacy single-tenant without registry)
    /// - `Pending` → `Err("pending")` — verification hasn't started yet
    /// - `Running` → `Err("running")` — verification is in progress
    /// - `Completed(all_passed: true)` → `Ok(())`
    /// - `Completed(all_passed: false)` → `Err("failed")` with failed level details
    #[instrument(skip_all, fields(otel.name = "entity.check_verification_gate", tenant = %tenant, entity_type))]
    pub fn check_verification_gate(
        &self,
        tenant: &TenantId,
        entity_type: &str,
    ) -> Result<(), VerificationGateError> {
        let registry = self.registry.read().unwrap();

        // If there's no tenant config in the registry, this is a legacy
        // single-tenant setup — allow operations for backward compatibility.
        let Some(tenant_config) = registry.get_tenant(tenant) else {
            return Ok(());
        };

        // If the entity type doesn't exist in the tenant, there's nothing to gate.
        if !tenant_config.entities.contains_key(entity_type) {
            return Ok(());
        }

        match tenant_config.verification.get(entity_type) {
            None => Ok(()),
            Some(VerificationStatus::Pending) => Err(VerificationGateError {
                entity_type: entity_type.to_string(),
                status: "pending".to_string(),
                message: format!(
                    "Verification has not started for entity type '{entity_type}'. \
                     Waiting for verification cascade to begin."
                ),
                failed_levels: None,
            }),
            Some(VerificationStatus::Running) => Err(VerificationGateError {
                entity_type: entity_type.to_string(),
                status: "running".to_string(),
                message: format!(
                    "Verification is currently running for entity type '{entity_type}'. \
                     Please wait for the cascade to complete."
                ),
                failed_levels: None,
            }),
            Some(VerificationStatus::Completed(result) | VerificationStatus::Restored(result)) => {
                if result.all_passed {
                    Ok(())
                } else {
                    let failed_levels: Vec<FailedLevelInfo> = result
                        .levels
                        .iter()
                        .filter(|l| !l.passed)
                        .map(|l| FailedLevelInfo {
                            level: l.level.clone(),
                            summary: l.summary.clone(),
                            details: l.details.clone(),
                        })
                        .collect();
                    Err(VerificationGateError {
                        entity_type: entity_type.to_string(),
                        status: "failed".to_string(),
                        message: format!(
                            "Verification failed for entity type '{entity_type}'. \
                             Fix the spec and re-push."
                        ),
                        failed_levels: Some(failed_levels),
                    })
                }
            }
        }
    }
}
