//! Tenant runtime-generation publication guards and request admission.

use super::*;

mod lease;
pub use lease::TenantGenerationLease;

/// Proof that one tenant's durable/runtime spec publication is serialized in
/// Temper's single-process runtime.
///
/// Callers acquire this before durable persistence and retain it until registry
/// publication and key-contract readiness both complete. Once armed, dropping
/// the guard deliberately does not reopen traffic: a storage error can be
/// outcome-ambiguous, so only a completed cutover may release the tenant gate.
pub struct SpecPublicationGuard {
    pub(in crate::state) tenant: String,
    pub(in crate::state) gated_tenants: Arc<RwLock<BTreeSet<String>>>,
    pub(in crate::state) publication_debts: Arc<RwLock<BTreeMap<String, String>>>,
    pub(in crate::state) generation_versions: Arc<RwLock<BTreeMap<String, u64>>>,
    pub(in crate::state) acquired_generation: u64,
    pub(in crate::state) intent_fingerprint: Option<String>,
    pub(in crate::state) inherited_debt: bool,
    pub(in crate::state) armed: bool,
    pub(in crate::state) _guard: tokio::sync::OwnedRwLockWriteGuard<()>,
}

impl SpecPublicationGuard {
    pub(in crate::state) fn validates(&self, tenant: &TenantId) -> Result<(), String> {
        if self.tenant != tenant.as_str() {
            return Err(format!(
                "spec publication guard for tenant {} cannot publish tenant {tenant}",
                self.tenant
            ));
        }
        if !self.armed {
            return Err(format!(
                "spec publication guard for tenant {tenant} was not armed before persistence"
            ));
        }
        Ok(())
    }

    pub(in crate::state) fn arm(
        &mut self,
        tenant: &TenantId,
        intent_fingerprint: &str,
    ) -> Result<(), String> {
        if self.tenant != tenant.as_str() {
            return Err(format!(
                "spec publication guard for tenant {} cannot arm tenant {tenant}",
                self.tenant
            ));
        }
        if intent_fingerprint.is_empty() {
            return Err(format!(
                "spec publication intent for tenant {tenant} must not be empty"
            ));
        }
        if self.armed {
            return Err(format!(
                "spec publication guard for tenant {tenant} is already armed"
            ));
        }
        let mut debts = self
            .publication_debts
            .write()
            .map_err(|error| format!("spec publication debt lock poisoned: {error}"))?;
        self.inherited_debt = match debts.get(tenant.as_str()) {
            Some(existing) if existing != intent_fingerprint => {
                return Err(format!(
                    "tenant {tenant} has an unresolved spec publication; retry its exact runtime generation before publishing a different one"
                ));
            }
            Some(_) => true,
            None => false,
        };
        self.gated_tenants
            .write()
            .map_err(|error| format!("spec publication gate lock poisoned: {error}"))?
            .insert(self.tenant.clone());
        debts.insert(self.tenant.clone(), intent_fingerprint.to_string());
        self.intent_fingerprint = Some(intent_fingerprint.to_string());
        self.armed = true;
        Ok(())
    }

    pub(in crate::state) fn release(&mut self, allow_inherited_debt: bool) -> Result<(), String> {
        if self.inherited_debt && !allow_inherited_debt {
            return Err(format!(
                "tenant {} requires its complete publication workflow to discharge the inherited runtime-generation debt",
                self.tenant
            ));
        }
        let intent_fingerprint = self.intent_fingerprint.as_deref().ok_or_else(|| {
            format!(
                "spec publication guard for tenant {} has no armed intent",
                self.tenant
            )
        })?;
        let mut debts = self
            .publication_debts
            .write()
            .map_err(|error| format!("spec publication debt lock poisoned: {error}"))?;
        if debts.get(&self.tenant).map(String::as_str) != Some(intent_fingerprint) {
            return Err(format!(
                "spec publication intent changed before tenant {} completed",
                self.tenant
            ));
        }
        self.gated_tenants
            .write()
            .map_err(|error| format!("spec publication gate lock poisoned: {error}"))?
            .remove(&self.tenant);
        debts.remove(&self.tenant);
        let mut versions = self
            .generation_versions
            .write()
            .expect("tenant generation version lock poisoned");
        let version = versions.entry(self.tenant.clone()).or_insert(0);
        *version = version
            .checked_add(1)
            .expect("tenant runtime generation counter overflowed");
        self.acquired_generation = *version;
        self.intent_fingerprint = None;
        self.inherited_debt = false;
        self.armed = false;
        Ok(())
    }
}

#[allow(deprecated)] // ADR-0025 Phase 4: RecordStore used until chain validation replaced
impl ServerState {
    /// Try to acquire the tenant coordinator before any durable spec publication
    /// and retain it through registry publication plus key-contract readiness.
    ///
    /// The coordinator is intentionally process-local because the current actor
    /// runtime supports exactly one server process. A distributed runtime must
    /// replace this with a durable lease spanning the full cutover.
    pub async fn begin_spec_publication(
        &self,
        tenant: &TenantId,
    ) -> Result<SpecPublicationGuard, String> {
        let tenant_lock = {
            let mut locks = self.spec_publication_locks.lock().await;
            locks
                .entry(tenant.as_str().to_string())
                .or_insert_with(|| Arc::new(tokio::sync::RwLock::new(())))
                .clone()
        };
        let guard = tenant_lock.try_write_owned().map_err(|_| {
            format!("tenant {tenant} runtime generation is busy; retry spec publication")
        })?;
        let acquired_generation = self.tenant_generation_version(tenant);
        Ok(SpecPublicationGuard {
            tenant: tenant.as_str().to_string(),
            gated_tenants: Arc::clone(&self.spec_publication_gated_tenants),
            publication_debts: Arc::clone(&self.spec_publication_debts),
            generation_versions: Arc::clone(&self.tenant_generation_versions),
            acquired_generation,
            intent_fingerprint: None,
            inherited_debt: false,
            armed: false,
            _guard: guard,
        })
    }

    /// Acquire a publication writer only if no complete generation committed
    /// since a caller released its stable request lease.
    pub async fn begin_spec_publication_after(
        &self,
        tenant: &TenantId,
        expected_generation: u64,
    ) -> Result<SpecPublicationGuard, String> {
        let guard = self.begin_spec_publication(tenant).await?;
        if guard.acquired_generation != expected_generation {
            return Err(format!(
                "tenant {tenant} advanced from runtime generation {expected_generation} to {} during publication handoff; retry the governed action",
                guard.acquired_generation
            ));
        }
        Ok(guard)
    }

    /// Drain independently forked work from the captured request generation,
    /// then acquire the publication writer if no completed cutover intervened.
    ///
    /// Generation-handoff hooks use this awaited path after releasing their
    /// request lease. A writer therefore cannot overtake post-commit work that
    /// was forked before the handoff, and the version check still rejects any
    /// different publication that completed while the caller waited.
    pub async fn begin_spec_publication_after_drain(
        &self,
        tenant: &TenantId,
        expected_generation: u64,
    ) -> Result<SpecPublicationGuard, String> {
        let tenant_lock = {
            let mut locks = self.spec_publication_locks.lock().await;
            locks
                .entry(tenant.as_str().to_string())
                .or_insert_with(|| Arc::new(tokio::sync::RwLock::new(())))
                .clone()
        };
        let writer = tenant_lock.write_owned().await;
        let acquired_generation = self.tenant_generation_version(tenant);
        if acquired_generation != expected_generation {
            return Err(format!(
                "tenant {tenant} advanced from runtime generation {expected_generation} to {acquired_generation} during publication handoff; retry the governed action"
            ));
        }
        Ok(SpecPublicationGuard {
            tenant: tenant.as_str().to_string(),
            gated_tenants: Arc::clone(&self.spec_publication_gated_tenants),
            publication_debts: Arc::clone(&self.spec_publication_debts),
            generation_versions: Arc::clone(&self.tenant_generation_versions),
            acquired_generation,
            intent_fingerprint: None,
            inherited_debt: false,
            armed: false,
            _guard: writer,
        })
    }

    /// Build an internal action context owned by an active publication writer.
    ///
    /// OS-app content and seed work runs after the new registry generation is
    /// installed but before the writer releases external traffic. The released
    /// lease is accepted only while this exact tenant remains gated at the
    /// writer's captured generation.
    pub fn spec_publication_dispatch_context(
        &self,
        guard: &SpecPublicationGuard,
        tenant: &TenantId,
        service: &str,
    ) -> Result<crate::request_context::AgentContext, String> {
        if guard.tenant != tenant.as_str() || !guard.armed {
            return Err(format!(
                "tenant {tenant} has no active publication writer for internal dispatch"
            ));
        }
        if guard.acquired_generation != self.tenant_generation_version(tenant)
            || !self.spec_publication_gated(tenant)
        {
            return Err(format!(
                "tenant {tenant} publication generation changed before internal dispatch"
            ));
        }
        Ok(
            crate::request_context::AgentContext::for_service(service)
                .with_tenant_generation_lease(TenantGenerationLease::publication_owned(
                    tenant,
                    guard.acquired_generation,
                )),
        )
    }

    /// Current complete live generation for one tenant.
    pub fn tenant_generation_version(&self, tenant: &TenantId) -> u64 {
        self.tenant_generation_versions
            .read()
            .expect("tenant generation version lock poisoned")
            .get(tenant.as_str())
            .copied()
            .unwrap_or(0)
    }

    /// Canonical digest naming the complete runtime generation a publication
    /// intends to make durable and live. Components are sorted by name and
    /// length-delimited, so callers can build the same intent on an exact retry
    /// without depending on map iteration or concatenation ambiguity.
    pub fn spec_publication_intent<'a>(
        kind: &str,
        components: impl IntoIterator<Item = (&'a str, &'a [u8])>,
    ) -> String {
        let mut components = components
            .into_iter()
            .map(|(name, value)| (name.to_string(), value.to_vec()))
            .collect::<Vec<_>>();
        components.sort_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)));
        let mut digest = Sha256::new();
        digest.update((kind.len() as u64).to_be_bytes());
        digest.update(kind.as_bytes());
        for (name, value) in components {
            digest.update((name.len() as u64).to_be_bytes());
            digest.update(name.as_bytes());
            digest.update((value.len() as u64).to_be_bytes());
            digest.update(value);
        }
        format!("{:x}", digest.finalize())
    }

    /// Hold a stable runtime generation while serving one tenant request.
    /// A queued publication writer prevents later requests from entering, and
    /// waits for earlier requests to finish before any registry/authz/WASM
    /// component can change.
    pub async fn begin_tenant_request(
        &self,
        tenant: &TenantId,
    ) -> tokio::sync::OwnedRwLockReadGuard<()> {
        let tenant_lock = {
            let mut locks = self.spec_publication_locks.lock().await;
            locks
                .entry(tenant.as_str().to_string())
                .or_insert_with(|| Arc::new(tokio::sync::RwLock::new(())))
                .clone()
        };
        tenant_lock.read_owned().await
    }

    /// Try to enter a stable tenant generation without waiting behind a
    /// publication. HTTP entry points use this bounded admission path and
    /// return a retryable response while a writer is active or queued.
    pub async fn try_begin_tenant_request(
        &self,
        tenant: &TenantId,
    ) -> Option<tokio::sync::OwnedRwLockReadGuard<()>> {
        let tenant_lock = {
            let mut locks = self.spec_publication_locks.lock().await;
            locks
                .entry(tenant.as_str().to_string())
                .or_insert_with(|| Arc::new(tokio::sync::RwLock::new(())))
                .clone()
        };
        tenant_lock.try_read_owned().ok()
    }

    /// Re-enter the exact runtime generation captured by detached work.
    ///
    /// The second version/gate check runs after acquiring the read side, so a
    /// writer can neither slip between validation and work nor let an old task
    /// borrow the next generation's registry and policy artifacts.
    #[cfg(test)]
    pub(crate) async fn try_begin_captured_tenant_generation(
        &self,
        tenant: &TenantId,
        captured_generation: u64,
    ) -> Option<TenantGenerationLease> {
        if self.spec_publication_gated(tenant)
            || self.tenant_generation_version(tenant) != captured_generation
        {
            return None;
        }
        let guard = self.try_begin_tenant_request(tenant).await?;
        if self.spec_publication_gated(tenant)
            || self.tenant_generation_version(tenant) != captured_generation
        {
            return None;
        }
        Some(TenantGenerationLease::new(
            tenant,
            guard,
            captured_generation,
        ))
    }

    /// Wait behind an active publication, then enter only if its resulting
    /// generation is exactly the one expected by queued background work.
    ///
    /// Publication-owned work uses this to defer effects produced from the
    /// pending registry generation until the writer makes that generation
    /// externally visible. An ambiguous publication leaves the tenant gated,
    /// so queued work fails closed after the writer releases its lock.
    pub(crate) async fn begin_captured_tenant_generation(
        &self,
        tenant: &TenantId,
        captured_generation: u64,
    ) -> Option<TenantGenerationLease> {
        let guard = self.begin_tenant_request(tenant).await;
        if self.spec_publication_gated(tenant)
            || self.tenant_generation_version(tenant) != captured_generation
        {
            return None;
        }
        Some(TenantGenerationLease::new(
            tenant,
            guard,
            captured_generation,
        ))
    }

    /// Activate an independently forked context for immediate detached work.
    /// Request-owned forks already hold the old generation and therefore drain
    /// before publication. Publication-owned tokens wait for the pending
    /// generation to become complete. A context without prior generation
    /// provenance enters whichever complete generation is current when it runs.
    pub(crate) async fn activate_immediate_tenant_work(
        &self,
        tenant: &TenantId,
        mut agent_ctx: crate::request_context::AgentContext,
    ) -> Option<crate::request_context::AgentContext> {
        match agent_ctx.tenant_generation_lease.as_ref() {
            Some(lease) if lease.is_held_for(tenant) => {
                if lease.captured_generation() != self.tenant_generation_version(tenant) {
                    return None;
                }
                return Some(agent_ctx);
            }
            Some(lease) if lease.is_publication_owned_for(tenant) => {
                let pending_generation = lease.captured_generation().checked_add(1)?;
                let lease = self
                    .begin_captured_tenant_generation(tenant, pending_generation)
                    .await?;
                agent_ctx.tenant_generation_lease = Some(lease);
                return Some(agent_ctx);
            }
            Some(_) => return None,
            None => {}
        }

        let guard = self.begin_tenant_request(tenant).await;
        if self.spec_publication_gated(tenant) {
            return None;
        }
        let generation = self.tenant_generation_version(tenant);
        agent_ctx.tenant_generation_lease =
            Some(TenantGenerationLease::new(tenant, guard, generation));
        Some(agent_ctx)
    }

    /// Whether an earlier outcome-ambiguous publication still keeps the tenant
    /// fail-closed after its writer guard has unwound.
    pub fn spec_publication_gated(&self, tenant: &TenantId) -> bool {
        self.spec_publication_gated_tenants
            .read()
            .expect("spec publication gate lock poisoned")
            .contains(tenant.as_str())
    }

    pub(crate) fn key_contract_activation_gated(
        &self,
        tenant: &TenantId,
        entity_type: &str,
    ) -> bool {
        self.spec_publication_gated(tenant)
            || self
                .activating_key_contracts
                .read()
                .expect("activating key contracts lock poisoned")
                .contains(&(tenant.as_str().to_string(), entity_type.to_string()))
    }
}
