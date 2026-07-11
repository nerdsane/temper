use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use temper_runtime::tenant::TenantId;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::MAX_RAW_BLOB_BYTES;

const DEFAULT_BUDGET_BYTES: usize = 4 * 1024 * 1024 * 1024;
const DEFAULT_BUDGET_UNIT_BYTES: usize = 1024 * 1024;
const DEFAULT_GLOBAL_CONCURRENCY: usize = 8;
const DEFAULT_PER_TENANT_CONCURRENCY: usize = 1;
const MAX_TENANT_ENTRIES: usize = 4096;
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_TOTAL_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const DEFAULT_THROUGHPUT_GRACE: Duration = Duration::from_secs(10);
const DEFAULT_THROUGHPUT_CHECK: Duration = Duration::from_secs(5);
const DEFAULT_MIN_BYTES_PER_SECOND: u64 = 64 * 1024;

/// Process-local admission budget for disk-backed raw Blob staging.
#[derive(Clone, Debug)]
pub(crate) struct BlobIngestBudget {
    permits: Arc<Semaphore>,
    global_slots: Arc<Semaphore>,
    tenant_slots: Arc<Mutex<BTreeMap<String, usize>>>,
    unit_bytes: usize,
    capacity_bytes: usize,
    per_tenant_concurrency: usize,
    progress_policy: BlobIngestProgressPolicy,
}

#[derive(Clone, Debug)]
pub(crate) struct BlobIngestProgressPolicy {
    pub(super) idle_timeout: Duration,
    pub(super) total_timeout: Duration,
    pub(super) throughput_grace: Duration,
    pub(super) throughput_check_interval: Duration,
    pub(super) min_bytes_per_second: u64,
}

pub(crate) struct BlobIngestPermit {
    byte_budget: Arc<Semaphore>,
    byte_permits: Vec<OwnedSemaphorePermit>,
    unit_bytes: usize,
    reserved_units: usize,
    _global_permit: OwnedSemaphorePermit,
    _tenant_permit: BlobTenantPermit,
}

struct BlobTenantPermit {
    slots: Arc<Mutex<BTreeMap<String, usize>>>,
    tenant: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlobIngestAdmissionError {
    ObjectTooLarge,
    BudgetExhausted,
    TenantBusy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BlobStageError {
    BodyStream(String),
    BodyExceedsDeclaredLength { declared: usize },
    BodyShorterThanDeclaredLength { declared: usize, received: usize },
    IdleTimeout { received: usize },
    TotalDeadline { received: usize },
    ThroughputTooLow { received: usize, required: u64 },
    StagingBudgetExhausted { received: usize },
    Storage(String),
}

impl std::fmt::Display for BlobStageError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BodyStream(error) => write!(formatter, "body stream failed: {error}"),
            Self::BodyExceedsDeclaredLength { declared } => {
                write!(
                    formatter,
                    "body bytes exceed declared Content-Length {declared}"
                )
            }
            Self::BodyShorterThanDeclaredLength { declared, received } => {
                write!(formatter, "expected {declared} body bytes, got {received}")
            }
            Self::IdleTimeout { received } => write!(
                formatter,
                "body made no progress before idle deadline at {received} bytes"
            ),
            Self::TotalDeadline { received } => {
                write!(
                    formatter,
                    "body exceeded total upload deadline at {received} bytes"
                )
            }
            Self::ThroughputTooLow { received, required } => write!(
                formatter,
                "body throughput was below the minimum: received {received} bytes, required {required}"
            ),
            Self::StagingBudgetExhausted { received } => write!(
                formatter,
                "aggregate staged bytes exhausted the process budget at {received} bytes"
            ),
            Self::Storage(error) => formatter.write_str(error),
        }
    }
}

impl BlobIngestProgressPolicy {
    fn runtime() -> Self {
        Self {
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            total_timeout: DEFAULT_TOTAL_TIMEOUT,
            throughput_grace: DEFAULT_THROUGHPUT_GRACE,
            throughput_check_interval: DEFAULT_THROUGHPUT_CHECK,
            min_bytes_per_second: DEFAULT_MIN_BYTES_PER_SECOND,
        }
    }

    #[cfg(test)]
    pub(crate) fn new(
        idle_timeout: Duration,
        total_timeout: Duration,
        throughput_grace: Duration,
        throughput_check_interval: Duration,
        min_bytes_per_second: u64,
    ) -> Self {
        assert!(
            !idle_timeout.is_zero(),
            "blob ingest idle timeout must be positive"
        );
        assert!(
            !total_timeout.is_zero(),
            "blob ingest total timeout must be positive"
        );
        assert!(
            !throughput_check_interval.is_zero(),
            "blob ingest throughput interval must be positive"
        );
        assert!(
            min_bytes_per_second > 0,
            "blob ingest minimum throughput must be positive"
        );
        Self {
            idle_timeout,
            total_timeout,
            throughput_grace,
            throughput_check_interval,
            min_bytes_per_second,
        }
    }
}

impl BlobIngestBudget {
    pub(crate) fn runtime() -> Self {
        Self::new(DEFAULT_BUDGET_BYTES, DEFAULT_BUDGET_UNIT_BYTES)
    }

    pub(crate) fn new(capacity_bytes: usize, unit_bytes: usize) -> Self {
        Self::with_limits(
            capacity_bytes,
            unit_bytes,
            DEFAULT_GLOBAL_CONCURRENCY,
            DEFAULT_PER_TENANT_CONCURRENCY,
            BlobIngestProgressPolicy::runtime(),
        )
    }

    pub(crate) fn with_limits(
        capacity_bytes: usize,
        unit_bytes: usize,
        global_concurrency: usize,
        per_tenant_concurrency: usize,
        progress_policy: BlobIngestProgressPolicy,
    ) -> Self {
        assert!(capacity_bytes > 0, "blob ingest budget must be positive");
        assert!(unit_bytes > 0, "blob ingest budget unit must be positive");
        assert!(
            capacity_bytes >= unit_bytes,
            "blob ingest budget must contain at least one complete accounting unit"
        );
        assert!(
            global_concurrency > 0,
            "global ingest concurrency must be positive"
        );
        assert!(
            per_tenant_concurrency > 0,
            "tenant ingest concurrency must be positive"
        );
        // Round down so unit accounting can never admit more actual staged
        // bytes than the configured capacity.
        let permit_count = capacity_bytes / unit_bytes;
        assert!(
            u32::try_from(permit_count).is_ok(),
            "blob ingest budget permit count must fit in u32"
        );
        Self {
            permits: Arc::new(Semaphore::new(permit_count)),
            global_slots: Arc::new(Semaphore::new(global_concurrency)),
            tenant_slots: Arc::new(Mutex::new(BTreeMap::new())),
            unit_bytes,
            capacity_bytes,
            per_tenant_concurrency,
            progress_policy,
        }
    }

    pub(crate) fn try_reserve(
        &self,
        tenant: &TenantId,
        declared_len: usize,
    ) -> Result<BlobIngestPermit, BlobIngestAdmissionError> {
        if declared_len > MAX_RAW_BLOB_BYTES || declared_len > self.capacity_bytes {
            return Err(BlobIngestAdmissionError::ObjectTooLarge);
        }
        let tenant_permit = self.try_reserve_tenant(tenant)?;
        let global_permit = self
            .global_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| BlobIngestAdmissionError::BudgetExhausted)?;
        let byte_permit = self
            .permits
            .clone()
            .try_acquire_owned()
            .map_err(|_| BlobIngestAdmissionError::BudgetExhausted)?;
        Ok(BlobIngestPermit {
            byte_budget: Arc::clone(&self.permits),
            byte_permits: vec![byte_permit],
            unit_bytes: self.unit_bytes,
            reserved_units: 1,
            _global_permit: global_permit,
            _tenant_permit: tenant_permit,
        })
    }

    pub(crate) fn capacity_bytes(&self) -> usize {
        self.capacity_bytes
    }

    pub(crate) fn progress_policy(&self) -> &BlobIngestProgressPolicy {
        &self.progress_policy
    }

    fn try_reserve_tenant(
        &self,
        tenant: &TenantId,
    ) -> Result<BlobTenantPermit, BlobIngestAdmissionError> {
        let tenant = tenant.as_str().to_string();
        let mut slots = self
            .tenant_slots
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if slots.get(&tenant).copied().unwrap_or(0) >= self.per_tenant_concurrency {
            return Err(BlobIngestAdmissionError::TenantBusy);
        }
        if !slots.contains_key(&tenant) && slots.len() >= MAX_TENANT_ENTRIES {
            return Err(BlobIngestAdmissionError::BudgetExhausted);
        }
        *slots.entry(tenant.clone()).or_insert(0) += 1;
        drop(slots);
        Ok(BlobTenantPermit {
            slots: Arc::clone(&self.tenant_slots),
            tenant,
        })
    }
}

impl BlobIngestPermit {
    pub(super) fn reserve_received_bytes(&mut self, received: usize) -> Result<(), BlobStageError> {
        let target_units = received.max(1).div_ceil(self.unit_bytes);
        if target_units <= self.reserved_units {
            return Ok(());
        }
        let additional = target_units - self.reserved_units;
        let additional = u32::try_from(additional)
            .map_err(|_| BlobStageError::StagingBudgetExhausted { received })?;
        let permit = self
            .byte_budget
            .clone()
            .try_acquire_many_owned(additional)
            .map_err(|_| BlobStageError::StagingBudgetExhausted { received })?;
        self.byte_permits.push(permit);
        self.reserved_units = target_units;
        Ok(())
    }
}

impl Drop for BlobTenantPermit {
    fn drop(&mut self) {
        let mut slots = self.slots.lock().unwrap_or_else(|error| error.into_inner());
        let Some(count) = slots.get_mut(&self.tenant) else {
            return;
        };
        *count = count.saturating_sub(1);
        if *count == 0 {
            slots.remove(&self.tenant);
        }
    }
}
