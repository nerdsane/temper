use super::super::trajectory::TrajectoryEntry;
use super::super::{DesignTimeEvent, ServerState};
use super::TenantMetadataBackend;

impl ServerState {
    /// Broadcast and persist a design-time event to the tenant's store.
    pub async fn emit_design_time_event(&self, event: DesignTimeEvent) -> Result<(), String> {
        if let Some(backend) = self.tenant_metadata_backend(&event.tenant).await {
            match backend {
                TenantMetadataBackend::Postgres(pool) => {
                    let created_at = temper_runtime::scheduler::sim_now();
                    sqlx::query(
                        "INSERT INTO design_time_events \
                         (kind, entity_type, tenant, summary, level, passed, step_number, total_steps, created_at) \
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
                    )
                    .bind(&event.kind)
                    .bind(&event.entity_type)
                    .bind(&event.tenant)
                    .bind(&event.summary)
                    .bind(event.level.as_deref())
                    .bind(event.passed)
                    .bind(event.step_number.map(i16::from))
                    .bind(event.total_steps.map(i16::from))
                    .bind(created_at)
                    .execute(&pool)
                    .await
                    .map_err(|e| {
                        format!(
                            "failed to persist design-time event {} for {}/{} in postgres: {e}",
                            event.kind, event.tenant, event.entity_type
                        )
                    })?;
                }
                TenantMetadataBackend::Turso(turso) => {
                    turso
                        .insert_design_time_event(
                            &event.kind,
                            &event.entity_type,
                            &event.tenant,
                            &event.summary,
                            event.level.as_deref(),
                            event.passed,
                            event.step_number.map(i64::from),
                            event.total_steps.map(i64::from),
                        )
                        .await
                        .map_err(|e| {
                            format!(
                                "failed to persist design-time event {} for {}/{} in turso: {e}",
                                event.kind, event.tenant, event.entity_type
                            )
                        })?;
                }
                TenantMetadataBackend::Redis => {}
            }
        }
        // Broadcast via SSE (keep for real-time UI).
        let _ = self.design_time_tx.send(event);
        // Emit observe refresh hints for specs/verification changes.
        let _ = self
            .observe_refresh_tx
            .send(crate::state::ObserveRefreshHint::Specs);
        let _ = self
            .observe_refresh_tx
            .send(crate::state::ObserveRefreshHint::Verification);
        let _ = self
            .observe_refresh_tx
            .send(crate::state::ObserveRefreshHint::OsApps);
        Ok(())
    }

    /// Persist a trajectory entry to the tenant's metadata store.
    pub async fn persist_trajectory_entry(&self, entry: &TrajectoryEntry) -> Result<(), String> {
        let Some((_backend, sink)) = self.trajectory_sink() else {
            return Ok(());
        };
        sink.persist_trajectory_entry(entry).await
    }

    /// Persist a pending decision to the tenant's storage backend.
    pub async fn persist_pending_decision(
        &self,
        decision: &super::super::PendingDecision,
    ) -> Result<(), String> {
        let Some(backend) = self.tenant_metadata_backend(&decision.tenant).await else {
            return Ok(());
        };

        let status_str = match decision.status {
            super::super::DecisionStatus::Pending => "pending",
            super::super::DecisionStatus::Approved => "approved",
            super::super::DecisionStatus::Denied => "denied",
            super::super::DecisionStatus::Expired => "expired",
        };
        let data_json = serde_json::to_string(decision)
            .map_err(|e| format!("failed to serialize decision {}: {e}", decision.id))?;
        match backend {
            TenantMetadataBackend::Postgres(pool) => {
                temper_store_postgres::PostgresEventStore::new(pool)
                    .upsert_pending_decision(&decision.id, &decision.tenant, status_str, &data_json)
                    .await
                    .map_err(|e| {
                        format!(
                            "failed to persist pending decision {} in postgres: {e}",
                            decision.id
                        )
                    })?;
            }
            TenantMetadataBackend::Turso(turso) => {
                turso
                    .upsert_pending_decision(&decision.id, &decision.tenant, status_str, &data_json)
                    .await
                    .map_err(|e| {
                        format!(
                            "failed to persist pending decision {} in turso: {e}",
                            decision.id
                        )
                    })?;
            }
            TenantMetadataBackend::Redis => {
                return Err(Self::redis_ephemeral_error("Pending decision persistence"));
            }
        }

        Ok(())
    }

    /// Whether `session_id` is a server-validated session grant for `agent_id`.
    ///
    /// True only when an APPROVED decision in `tenant` carries an approved scope
    /// with `duration = session`, the same session id, and the same agent — a
    /// human explicitly approved this principal for this session. This is the
    /// server-side record that lets a caller-asserted session header become a
    /// Cedar input (ADR-0157); without it the header stays telemetry, so a
    /// session-scoped permit can only ever match the principal it was approved
    /// for. Fails closed: no backend or a storage error means "not verified".
    pub async fn session_grant_verified(
        &self,
        tenant: &str,
        agent_id: &str,
        session_id: &str,
    ) -> bool {
        let Some(backend) = self.tenant_metadata_backend(tenant).await else {
            return false;
        };
        let blobs = match backend {
            TenantMetadataBackend::Postgres(pool) => {
                temper_store_postgres::PostgresEventStore::new(pool)
                    .load_approved_session_decisions(tenant, session_id)
                    .await
                    .map_err(|e| e.to_string())
            }
            TenantMetadataBackend::Turso(turso) => turso
                .load_approved_session_decisions(tenant, session_id)
                .await
                .map_err(|e| e.to_string()),
            TenantMetadataBackend::Redis => {
                Err(Self::redis_ephemeral_error("Session grant validation"))
            }
        };
        let blobs = match blobs {
            Ok(blobs) => blobs,
            Err(e) => {
                tracing::warn!(tenant, session_id, error = %e, "session grant lookup failed; treating as unverified");
                return false;
            }
        };
        blobs.iter().any(|blob| {
            serde_json::from_str::<super::super::PendingDecision>(blob)
                .map(|d| {
                    d.status == super::super::DecisionStatus::Approved
                        && d.agent_id == agent_id
                        && d.approved_scope.as_ref().is_some_and(|scope| {
                            scope.duration == temper_authz::DurationScope::Session
                                && scope.session_id.as_deref() == Some(session_id)
                        })
                })
                .unwrap_or(false)
        })
    }

    /// Upsert an encrypted secret in the persistence backend.
    pub async fn upsert_secret(
        &self,
        tenant: &str,
        key_name: &str,
        ciphertext: &[u8],
        nonce: &[u8],
    ) -> Result<(), String> {
        let Some(backend) = self.tenant_metadata_backend(tenant).await else {
            return Ok(());
        };

        match backend {
            TenantMetadataBackend::Postgres(pool) => {
                sqlx::query(
                    "INSERT INTO tenant_secrets (tenant, key_name, ciphertext, nonce, created_at, updated_at) \
                     VALUES ($1, $2, $3, $4, now(), now()) \
                     ON CONFLICT (tenant, key_name) DO UPDATE SET \
                         ciphertext = EXCLUDED.ciphertext, \
                         nonce = EXCLUDED.nonce, \
                         updated_at = now()",
                )
                .bind(tenant)
                .bind(key_name)
                .bind(ciphertext)
                .bind(nonce)
                .execute(&pool)
                .await
                .map_err(|e| format!("failed to upsert secret {tenant}/{key_name}: {e}"))?;
                Ok(())
            }
            TenantMetadataBackend::Turso(turso) => turso
                .upsert_secret(tenant, key_name, ciphertext, nonce)
                .await
                .map_err(|e| format!("failed to upsert secret {tenant}/{key_name} in turso: {e}")),
            TenantMetadataBackend::Redis => Err(Self::redis_ephemeral_error("Secret persistence")),
        }
    }

    /// Delete a secret from the persistence backend.
    pub async fn delete_secret(&self, tenant: &str, key_name: &str) -> Result<bool, String> {
        let Some(backend) = self.tenant_metadata_backend(tenant).await else {
            return Ok(false);
        };

        match backend {
            TenantMetadataBackend::Postgres(pool) => {
                let result =
                    sqlx::query("DELETE FROM tenant_secrets WHERE tenant = $1 AND key_name = $2")
                        .bind(tenant)
                        .bind(key_name)
                        .execute(&pool)
                        .await
                        .map_err(|e| format!("failed to delete secret {tenant}/{key_name}: {e}"))?;
                Ok(result.rows_affected() > 0)
            }
            TenantMetadataBackend::Turso(turso) => turso
                .delete_secret(tenant, key_name)
                .await
                .map_err(|e| format!("failed to delete secret {tenant}/{key_name} in turso: {e}")),
            TenantMetadataBackend::Redis => Err(Self::redis_ephemeral_error("Secret deletion")),
        }
    }

    /// Load all secrets for a tenant from persistence, decrypt, and cache.
    pub async fn load_tenant_secrets(&self, tenant: &str) -> Result<usize, String> {
        let Some(vault) = self.secrets_vault.as_ref() else {
            return Ok(0);
        };
        let Some(backend) = self.tenant_metadata_backend(tenant).await else {
            return Ok(0);
        };

        match backend {
            TenantMetadataBackend::Postgres(pool) => {
                let rows: Vec<(String, Vec<u8>, Vec<u8>)> = sqlx::query_as(
                    "SELECT key_name, ciphertext, nonce FROM tenant_secrets WHERE tenant = $1",
                )
                .bind(tenant)
                .fetch_all(&pool)
                .await
                .map_err(|e| format!("failed to load secrets for tenant {tenant}: {e}"))?;

                let mut count = 0;
                for (key_name, ciphertext, nonce) in &rows {
                    match vault.decrypt(ciphertext, nonce) {
                        Ok(plaintext) => {
                            let value = String::from_utf8(plaintext).map_err(|e| {
                                format!("secret {key_name} is not valid UTF-8: {e}")
                            })?;
                            vault.cache_secret(tenant, key_name, value)?;
                            count += 1;
                        }
                        Err(e) => {
                            tracing::warn!(
                                tenant,
                                key_name,
                                error = %e,
                                "failed to decrypt secret, skipping"
                            );
                        }
                    }
                }
                Ok(count)
            }
            TenantMetadataBackend::Turso(turso) => {
                let rows = turso.load_secrets_for_tenant(tenant).await.map_err(|e| {
                    format!("failed to load secrets for tenant {tenant} from turso: {e}")
                })?;

                let mut count = 0;
                for (key_name, ciphertext, nonce) in &rows {
                    match vault.decrypt(ciphertext, nonce) {
                        Ok(plaintext) => {
                            let value = String::from_utf8(plaintext).map_err(|e| {
                                format!("secret {key_name} is not valid UTF-8: {e}")
                            })?;
                            vault.cache_secret(tenant, key_name, value)?;
                            count += 1;
                        }
                        Err(e) => {
                            tracing::warn!(
                                tenant,
                                key_name,
                                error = %e,
                                "failed to decrypt secret, skipping"
                            );
                        }
                    }
                }
                Ok(count)
            }
            TenantMetadataBackend::Redis => Err(Self::redis_ephemeral_error("Secret loading")),
        }
    }
}

#[cfg(test)]
mod tests {
    use temper_runtime::ActorSystem;
    use temper_store_turso::TursoEventStore;

    use crate::registry::SpecRegistry;
    use crate::secrets::vault::SecretsVault;
    use crate::state::ServerState;
    use crate::storage::StorageStack;

    fn make_state() -> ServerState {
        let system = ActorSystem::new("test-secrets-persistence");
        ServerState::from_registry(system, SpecRegistry::new())
            .with_secrets_vault(SecretsVault::new(&[7u8; 32]))
    }

    #[tokio::test]
    async fn turso_secret_round_trip() {
        let db_path =
            std::env::temp_dir().join(format!("temper-secrets-{}.db", uuid::Uuid::new_v4())); // determinism-ok: test-only temp file
        let db_url = format!("file:{}", db_path.display());
        let store = TursoEventStore::new(&db_url, None)
            .await
            .expect("create local turso db");

        let mut state = make_state();
        state.set_storage_stack(StorageStack::from_turso(store));

        let vault = state.secrets_vault.as_ref().expect("vault configured");
        let (ciphertext, nonce) = vault.encrypt(b"secret-value").expect("encrypt");

        // Upsert secret.
        state
            .upsert_secret("tenant-a", "API_KEY", &ciphertext, &nonce)
            .await
            .expect("turso secret upsert should succeed");

        // Load and decrypt.
        let loaded = state
            .load_tenant_secrets("tenant-a")
            .await
            .expect("turso secret load should succeed");
        assert_eq!(loaded, 1, "should have loaded 1 secret");

        // Verify the decrypted value is cached.
        let cached = vault
            .get_secret("tenant-a", "API_KEY")
            .expect("secret should be cached");
        assert_eq!(cached, "secret-value");

        // Delete secret.
        let deleted = state
            .delete_secret("tenant-a", "API_KEY")
            .await
            .expect("turso secret delete should succeed");
        assert!(deleted, "should have deleted 1 row");

        let _ = std::fs::remove_file(db_path); // determinism-ok: test-only cleanup
    }
}

#[cfg(test)]
mod session_grant_tests {
    use temper_authz::{DurationScope, PolicyScopeMatrix};
    use temper_runtime::ActorSystem;

    use crate::registry::SpecRegistry;
    use crate::state::{DecisionStatus, PendingDecision, ServerState};
    use crate::storage::StorageStack;

    async fn state_with_turso(test_name: &str) -> ServerState {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "temper-session-grant-{test_name}-{}.db",
            uuid::Uuid::new_v4() // determinism-ok: test-only temporary database filename
        ));
        let turso =
            temper_store_turso::TursoEventStore::new(&format!("file:{}", path.display()), None)
                .await
                .expect("create local turso db");
        let mut state =
            ServerState::from_registry(ActorSystem::new("session-grant-test"), SpecRegistry::new());
        state.set_storage_stack(StorageStack::from_turso(turso));
        state
    }

    fn decision(
        tenant: &str,
        agent_id: &str,
        status: DecisionStatus,
        scope: Option<PolicyScopeMatrix>,
    ) -> PendingDecision {
        let mut d = PendingDecision::from_denial(
            tenant,
            agent_id,
            "Delete",
            "Order",
            "order-1",
            serde_json::json!({}),
            "denied by policy",
            None,
        );
        d.status = status;
        d.approved_scope = scope;
        d
    }

    fn session_scope(session_id: &str) -> PolicyScopeMatrix {
        let mut scope = PolicyScopeMatrix::default_for(Some("operator"));
        scope.duration = DurationScope::Session;
        scope.session_id = Some(session_id.to_string());
        scope
    }

    /// The grant is exact: approved decision, same agent, same session, session
    /// duration. Anything less must not turn a caller-asserted header into a
    /// Cedar input (ADR-0157) — each negative arm below is one relaxation.
    #[tokio::test]
    async fn a_session_grant_binds_exactly_one_agent_and_session() {
        let state = state_with_turso("exact-binding").await;

        state
            .persist_pending_decision(&decision(
                "default",
                "agent-a",
                DecisionStatus::Approved,
                Some(session_scope("sess-approved")),
            ))
            .await
            .expect("persist approved grant");

        assert!(
            state
                .session_grant_verified("default", "agent-a", "sess-approved")
                .await,
            "the approved (agent, session) pair must verify"
        );
        assert!(
            !state
                .session_grant_verified("default", "agent-b", "sess-approved")
                .await,
            "another agent asserting the approved session must not verify"
        );
        assert!(
            !state
                .session_grant_verified("default", "agent-a", "sess-other")
                .await,
            "the approved agent asserting a different session must not verify"
        );
        assert!(
            !state
                .session_grant_verified("other-tenant", "agent-a", "sess-approved")
                .await,
            "the grant must not verify outside its tenant"
        );
    }

    #[tokio::test]
    async fn an_unapproved_or_unscoped_decision_is_not_a_grant() {
        let state = state_with_turso("not-a-grant").await;

        // Still pending: the human has not approved anything.
        state
            .persist_pending_decision(&decision(
                "default",
                "agent-a",
                DecisionStatus::Pending,
                Some(session_scope("sess-pending")),
            ))
            .await
            .expect("persist pending decision");
        assert!(
            !state
                .session_grant_verified("default", "agent-a", "sess-pending")
                .await,
            "a pending decision must not act as a session grant"
        );

        // Approved, but not session-scoped: an Always-duration approval names no
        // session, so no session assertion may borrow it.
        let mut always = PolicyScopeMatrix::default_for(Some("operator"));
        always.session_id = Some("sess-always".to_string());
        state
            .persist_pending_decision(&decision(
                "default",
                "agent-a",
                DecisionStatus::Approved,
                Some(always),
            ))
            .await
            .expect("persist always-duration decision");
        assert!(
            !state
                .session_grant_verified("default", "agent-a", "sess-always")
                .await,
            "an approval without session duration must not act as a session grant"
        );
    }
}
