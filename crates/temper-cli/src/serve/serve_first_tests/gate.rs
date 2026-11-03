//! A policy store that records which tenants a boot reads, and holds the
//! reads of chosen tenants until the test lets them through.
//!
//! A tenant's Cedar policies are the last thing a request needs before it
//! can be answered right, so holding that read holds the tenant's restore at
//! the point where a wrong answer (a default-deny 403) would be possible.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use temper_server::storage::{PolicyStore, PolicyStoreRow};
use tokio::sync::{Notify, mpsc};

/// What the test sees and controls of a boot's policy reads.
pub(super) struct Gate {
    inner: Mutex<GateState>,
    changed: Notify,
    waiting: mpsc::UnboundedSender<String>,
}

#[derive(Default)]
struct GateState {
    /// Set once the server serves; reads before it are the boot's own.
    serving: bool,
    /// Every tenant read so far, and whether the server served by then.
    reads: Vec<(String, bool)>,
    /// Tenants whose reads wait, once the server serves, until released.
    held: BTreeSet<String>,
    released: BTreeSet<String>,
    /// Tenants whose reads fail, as a store that cannot be read.
    failing: BTreeSet<String>,
}

impl Gate {
    /// A gate that holds `held` and fails `failing`, and the receiver that
    /// names each held tenant as its read starts waiting.
    pub(super) fn new(
        held: &[String],
        failing: &[String],
    ) -> (Arc<Self>, mpsc::UnboundedReceiver<String>) {
        let (waiting, waits) = mpsc::unbounded_channel();
        let gate = Self {
            inner: Mutex::new(GateState {
                held: held.iter().cloned().collect(),
                failing: failing.iter().cloned().collect(),
                ..GateState::default()
            }),
            changed: Notify::new(),
            waiting,
        };
        (Arc::new(gate), waits)
    }

    /// The server serves from now on.
    pub(super) fn serving(&self) {
        self.inner.lock().expect("gate lock").serving = true;
    }

    /// The tenants read before the server served.
    pub(super) fn read_before_serving(&self) -> BTreeSet<String> {
        let inner = self.inner.lock().expect("gate lock");
        inner
            .reads
            .iter()
            .filter(|(_, serving)| !serving)
            .map(|(tenant, _)| tenant.clone())
            .collect()
    }

    /// Let `tenant`'s waiting and later reads through.
    pub(super) fn release(&self, tenant: &str) {
        self.inner
            .lock()
            .expect("gate lock")
            .released
            .insert(tenant.to_string());
        self.changed.notify_waiters();
    }

    /// Record a read of `tenant`, and wait while it is held. Err when the
    /// tenant's reads fail.
    async fn pass(&self, tenant: &str) -> Result<(), String> {
        let held = {
            let mut inner = self.inner.lock().expect("gate lock");
            let serving = inner.serving;
            inner.reads.push((tenant.to_string(), serving));
            if inner.failing.contains(tenant) {
                return Err(format!("the policy store could not be read for {tenant}"));
            }
            serving && inner.held.contains(tenant) && !inner.released.contains(tenant)
        };
        if !held {
            return Ok(());
        }
        let _ = self.waiting.send(tenant.to_string());
        loop {
            let changed = self.changed.notified();
            if self
                .inner
                .lock()
                .expect("gate lock")
                .released
                .contains(tenant)
            {
                return Ok(());
            }
            changed.await;
        }
    }
}

/// The server's policy store, with every per-tenant read going through a
/// [`Gate`].
pub(super) struct GatedPolicies {
    pub(super) inner: Arc<dyn PolicyStore>,
    pub(super) gate: Arc<Gate>,
}

#[async_trait::async_trait]
impl PolicyStore for GatedPolicies {
    async fn save_policy(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<bool, String> {
        self.inner
            .save_policy(tenant, policy_id, cedar_text, created_by)
            .await
    }

    async fn load_policies_for_tenant(&self, tenant: &str) -> Result<Vec<PolicyStoreRow>, String> {
        self.gate.pass(tenant).await?;
        self.inner.load_policies_for_tenant(tenant).await
    }

    async fn load_all_policies(&self) -> Result<Vec<PolicyStoreRow>, String> {
        self.inner.load_all_policies().await
    }

    async fn toggle_policy_enabled(
        &self,
        tenant: &str,
        policy_id: &str,
        enabled: bool,
    ) -> Result<bool, String> {
        self.inner
            .toggle_policy_enabled(tenant, policy_id, enabled)
            .await
    }

    async fn update_policy_text(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<bool, String> {
        self.inner
            .update_policy_text(tenant, policy_id, cedar_text, created_by)
            .await
    }

    async fn replace_policy_if_hash(
        &self,
        tenant: &str,
        policy_id: &str,
        expected_hash: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<bool, String> {
        self.inner
            .replace_policy_if_hash(tenant, policy_id, expected_hash, cedar_text, created_by)
            .await
    }

    async fn replace_tenant_policies(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<(), String> {
        self.inner
            .replace_tenant_policies(tenant, policy_id, cedar_text, created_by)
            .await
    }

    async fn delete_policy(&self, tenant: &str, policy_id: &str) -> Result<(), String> {
        self.inner.delete_policy(tenant, policy_id).await
    }
}
