//! Stable-generation ownership carried by requests and immediate obligations.

use super::*;

/// Cloneable owner for the stable tenant generation held by an HTTP request.
///
/// A bound action whose post-action hook publishes the same tenant may release
/// this lease after its governed action completes, then acquire the publication
/// writer without attempting an impossible read-to-write lock upgrade.
#[derive(Clone)]
pub struct TenantGenerationLease {
    tenant: String,
    guard: Arc<Mutex<Option<Arc<tokio::sync::OwnedRwLockReadGuard<()>>>>>,
    captured_generation: u64,
    provenance: TenantGenerationLeaseProvenance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TenantGenerationLeaseProvenance {
    Request,
    PublicationOwned,
}

impl std::fmt::Debug for TenantGenerationLease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TenantGenerationLease")
            .field("tenant", &self.tenant)
            .field("captured_generation", &self.captured_generation)
            .field("held", &self.is_held())
            .field("provenance", &self.provenance)
            .finish()
    }
}

impl TenantGenerationLease {
    pub(crate) fn new(
        tenant: &TenantId,
        guard: tokio::sync::OwnedRwLockReadGuard<()>,
        captured_generation: u64,
    ) -> Self {
        Self {
            tenant: tenant.as_str().to_string(),
            guard: Arc::new(Mutex::new(Some(Arc::new(guard)))),
            captured_generation,
            provenance: TenantGenerationLeaseProvenance::Request,
        }
    }

    pub(crate) fn publication_owned(tenant: &TenantId, captured_generation: u64) -> Self {
        Self {
            tenant: tenant.as_str().to_string(),
            guard: Arc::new(Mutex::new(None)),
            captured_generation,
            provenance: TenantGenerationLeaseProvenance::PublicationOwned,
        }
    }

    /// Fork an independently releasable proof for immediate detached work.
    /// Request forks keep the read side alive so a queued publisher drains the
    /// obligation before cutover. Publication-owned work retains its explicit
    /// pending-generation provenance and waits for the writer to complete.
    pub(crate) fn fork_immediate(&self, tenant: &TenantId) -> Option<Self> {
        if !self.belongs_to(tenant) {
            return None;
        }
        match self.provenance {
            TenantGenerationLeaseProvenance::Request => {
                let guard = self
                    .guard
                    .lock()
                    .expect("tenant generation lease lock poisoned")
                    .clone()?;
                Some(Self {
                    tenant: self.tenant.clone(),
                    guard: Arc::new(Mutex::new(Some(guard))),
                    captured_generation: self.captured_generation,
                    provenance: TenantGenerationLeaseProvenance::Request,
                })
            }
            TenantGenerationLeaseProvenance::PublicationOwned => {
                Some(Self::publication_owned(tenant, self.captured_generation))
            }
        }
    }

    /// Runtime-generation token observed while this request held its read lease.
    pub fn captured_generation(&self) -> u64 {
        self.captured_generation
    }

    pub(crate) fn is_held_for(&self, tenant: &TenantId) -> bool {
        self.belongs_to(tenant) && self.is_held()
    }

    pub(crate) fn belongs_to(&self, tenant: &TenantId) -> bool {
        self.tenant == tenant.as_str()
    }

    pub(crate) fn is_publication_owned_for(&self, tenant: &TenantId) -> bool {
        self.belongs_to(tenant)
            && self.provenance == TenantGenerationLeaseProvenance::PublicationOwned
    }

    fn is_held(&self) -> bool {
        self.guard
            .lock()
            .expect("tenant generation lease lock poisoned")
            .is_some()
    }

    /// Release the request's stable generation. Repeated release is a no-op.
    pub fn release(&self) {
        self.guard
            .lock()
            .expect("tenant generation lease lock poisoned")
            .take();
    }
}
