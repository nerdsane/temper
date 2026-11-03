//! Tenants a server restores after it starts serving.
//!
//! A server with many tenants restores only its own before it answers, and
//! the rest after it serves, one at a time (`temper serve` walks them). Each
//! of those tenants is *starting* until its restore ends. A request for a
//! starting tenant moves it to the front of the queue and waits for its
//! restore attempt to end, so it gets the answer the restored tenant gives,
//! never one from a tenant half restored: no 404 for a type not yet
//! registered, no empty list, no 401 for a credential not yet loaded, no
//! default-deny 403 from policies not yet read.
//!
//! An attempt that fails, because a read of the tenant's store failed, leaves
//! it starting: a request then answers 503 starting with the reason at once,
//! and the tenant is tried again after a wait.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use temper_runtime::tenant::TenantId;
use tokio::sync::Notify;

/// The wait before the tenants whose restore failed are tried again. It
/// doubles up to [`RETRY_MAX`].
pub const RETRY_FIRST: Duration = Duration::from_secs(1);
/// The longest wait between two tries. The reason for the ceiling is the
/// store: a store that stays down gets one read a minute per tenant, not a
/// tight loop of them. A request for the tenant does not wait for it: it
/// moves the tenant to the front and its restore is tried at once.
pub const RETRY_MAX: Duration = Duration::from_secs(60);

/// The tenants still to restore after the server serves, in the order they
/// are restored, and what their requests wait on.
#[derive(Default)]
pub struct TenantRestore {
    queue: Mutex<Queue>,
    /// Woken when an attempt ends.
    ended: Notify,
    /// Woken when a request moves a tenant to the front.
    wanted: Notify,
}

#[derive(Default)]
struct Queue {
    /// Tenants to restore, the next one first.
    waiting: VecDeque<TenantId>,
    /// The tenant whose restore is under way.
    current: Option<TenantId>,
    /// Tenants whose last attempt failed, waiting for their retry.
    retry: BTreeSet<TenantId>,
    /// Why the last attempt of each starting tenant failed.
    failures: BTreeMap<TenantId, String>,
    /// Attempts ended, for each starting tenant.
    attempts: BTreeMap<TenantId, u64>,
    /// The wait before the next retry.
    retry_wait: Option<Duration>,
}

impl Queue {
    fn starting(&self, tenant: &TenantId) -> bool {
        self.current.as_ref() == Some(tenant)
            || self.retry.contains(tenant)
            || self.waiting.contains(tenant)
    }
}

/// Where a request's tenant stands once any restore it waited for ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Standing {
    /// Not starting: restored, or never left for after serving.
    Restored,
    /// Still starting: its restore attempt failed for `reason`.
    Starting {
        /// Why the tenant's last restore attempt failed.
        reason: String,
        /// When its restore is tried again.
        retry_after: Duration,
    },
}

impl TenantRestore {
    /// Leave `tenants` for after the server serves; they are starting until
    /// each is restored, in this order.
    pub fn begin(&self, tenants: impl IntoIterator<Item = TenantId>) {
        let mut queue = self.queue.lock().expect("restore queue lock poisoned");
        for tenant in tenants {
            assert!(
                !queue.starting(&tenant),
                "tenant {tenant} is left for after serving twice"
            );
            queue.waiting.push_back(tenant);
        }
    }

    /// Whether `tenant` is starting.
    pub fn is_starting(&self, tenant: &TenantId) -> bool {
        self.queue
            .lock()
            .expect("restore queue lock poisoned")
            .starting(tenant)
    }

    /// How many tenants are starting.
    pub fn starting_count(&self) -> usize {
        let queue = self.queue.lock().expect("restore queue lock poisoned");
        queue.waiting.len() + queue.retry.len() + usize::from(queue.current.is_some())
    }

    /// The next tenant to restore, waiting for the retry of those that failed
    /// when no other is left. None once every tenant is restored.
    pub async fn next(&self) -> Option<TenantId> {
        loop {
            let wait = {
                let mut queue = self.queue.lock().expect("restore queue lock poisoned");
                assert!(queue.current.is_none(), "one restore at a time");
                if let Some(tenant) = queue.waiting.pop_front() {
                    queue.current = Some(tenant.clone());
                    return Some(tenant);
                }
                if queue.retry.is_empty() {
                    return None;
                }
                *queue.retry_wait.get_or_insert(RETRY_FIRST)
            };
            tokio::select! {
                () = tokio::time::sleep(wait) => { // determinism-ok: retry backoff on the runtime clock
                    let mut queue = self.queue.lock().expect("restore queue lock poisoned");
                    let retry = std::mem::take(&mut queue.retry);
                    queue.waiting.extend(retry);
                    queue.retry_wait = Some((wait * 2).min(RETRY_MAX));
                }
                () = self.wanted.notified() => {}
            }
        }
    }

    /// The restore of `tenant` [`Self::next`] gave ended: restored, or failed
    /// for the reason given, which leaves it starting until it is tried again.
    pub fn ended(&self, tenant: &TenantId, outcome: Result<(), String>) {
        {
            let mut queue = self.queue.lock().expect("restore queue lock poisoned");
            assert_eq!(
                queue.current.as_ref(),
                Some(tenant),
                "only the restore under way ends"
            );
            queue.current = None;
            match outcome {
                Ok(()) => {
                    queue.attempts.remove(tenant);
                    queue.failures.remove(tenant);
                    if queue.retry.is_empty() {
                        queue.retry_wait = None;
                    }
                }
                Err(reason) => {
                    *queue.attempts.entry(tenant.clone()).or_default() += 1;
                    queue.failures.insert(tenant.clone(), reason);
                    queue.retry.insert(tenant.clone());
                }
            }
        }
        self.ended.notify_waiters();
    }

    /// Wait for `tenant` if it is starting: move it to the front, unless its
    /// restore is under way, and wait for that attempt to end.
    pub async fn wait_for(&self, tenant: &TenantId) -> Standing {
        let mut ended = std::pin::pin!(self.ended.notified());
        ended.as_mut().enable();
        let seen = {
            let mut queue = self.queue.lock().expect("restore queue lock poisoned");
            if !queue.starting(tenant) {
                return Standing::Restored;
            }
            if queue.current.as_ref() != Some(tenant) {
                queue.waiting.retain(|waiting| waiting != tenant);
                queue.retry.remove(tenant);
                queue.waiting.push_front(tenant.clone());
                self.wanted.notify_one();
            }
            queue.attempts.get(tenant).copied().unwrap_or(0)
        };
        loop {
            ended.as_mut().await;
            ended.set(self.ended.notified());
            ended.as_mut().enable();
            let queue = self.queue.lock().expect("restore queue lock poisoned");
            if !queue.starting(tenant) {
                return Standing::Restored;
            }
            if queue.attempts.get(tenant).copied().unwrap_or(0) > seen {
                return Standing::Starting {
                    reason: queue
                        .failures
                        .get(tenant)
                        .cloned()
                        .expect("a failed attempt keeps its reason"),
                    retry_after: queue.retry_wait.unwrap_or(RETRY_FIRST),
                };
            }
        }
    }
}
