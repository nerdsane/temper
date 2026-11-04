//! Trims each tenant's WASM invocation log off the module call path.
//!
//! Recording an invocation only inserts and tells the pruner. A log is
//! trimmed once it holds a full batch of rows over its bound, so a delete
//! runs once per [`WASM_INVOCATIONS_PRUNE_BATCH_ROWS`] inserts, never for a
//! single row, and a tenant's log may sit up to one batch over
//! [`WASM_INVOCATIONS_KEPT_PER_TENANT`] until then. Right after a trim it
//! holds exactly the bound.
//!
//! The pruner counts inserts per tenant since its last look. It asks the
//! store for one row count when it first sees a tenant, from the drain task
//! and never on an insert. One drain task, started by the first mark and ended
//! when nothing needs a look, handles tenants one at a time. No timer runs
//! when the log is quiet.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use temper_runtime::persistence::{
    WASM_INVOCATIONS_KEPT_PER_TENANT, WASM_INVOCATIONS_PRUNE_BATCH_ROWS,
};

#[derive(Default)]
struct Pending {
    /// Rows a tenant's log holds over the bound; absent until first counted.
    over: BTreeMap<String, i64>,
    /// Tenants that need a look: uncounted, or a full batch over.
    dirty: BTreeSet<String>,
    draining: bool,
}

/// Tenants whose invocation log may be a full batch over its bound.
#[derive(Clone, Default)]
pub(crate) struct WasmLogPruner {
    pending: Arc<Mutex<Pending>>,
}

impl WasmLogPruner {
    /// Note one insert for `tenant`. True when the caller must start the
    /// drain task.
    pub(crate) fn mark(&self, tenant: &str) -> bool {
        let mut pending = self.pending.lock().expect("pruner lock");
        let needs_look = match pending.over.get_mut(tenant) {
            Some(over) => {
                *over += 1;
                *over >= WASM_INVOCATIONS_PRUNE_BATCH_ROWS
            }
            None => true,
        };
        if !needs_look {
            return false;
        }
        pending.dirty.insert(tenant.to_string());
        !std::mem::replace(&mut pending.draining, true)
    }

    /// Take the next tenant to look at; when none is left the drain task is
    /// over.
    pub(crate) fn next(&self) -> Option<String> {
        let mut pending = self.pending.lock().expect("pruner lock");
        let next = pending.dirty.pop_first();
        if next.is_none() {
            pending.draining = false;
        }
        next
    }

    /// Rows `tenant` holds over the bound, when known.
    fn over(&self, tenant: &str) -> Option<i64> {
        self.pending
            .lock()
            .expect("pruner lock")
            .over
            .get(tenant)
            .copied()
    }

    fn set_over(&self, tenant: &str, over: i64) {
        self.pending
            .lock()
            .expect("pruner lock")
            .over
            .insert(tenant.to_string(), over);
    }
}

impl crate::state::ServerState {
    /// Note that `tenant` logged an invocation and wake the pruner if that
    /// leaves its log a full batch over the bound.
    pub(crate) fn schedule_wasm_log_prune(&self, tenant: &str) {
        if !self.wasm_log_pruner.mark(tenant) {
            return;
        }
        let state = self.clone();
        tokio::spawn(
            // determinism-ok: background trim of the WASM invocation log
            async move {
                while let Some(tenant) = state.wasm_log_pruner.next() {
                    state.trim_wasm_log_if_full_batch_over(&tenant).await;
                }
            },
        );
    }

    async fn trim_wasm_log_if_full_batch_over(&self, tenant: &str) {
        let Some(store) = self.metadata_store_for_tenant(tenant).await else {
            return;
        };
        let pruner = &self.wasm_log_pruner;
        let over = match pruner.over(tenant) {
            Some(over) => over,
            None => match store.count_wasm_invocations(tenant).await {
                Ok(count) => {
                    let over = (count - WASM_INVOCATIONS_KEPT_PER_TENANT).max(0);
                    pruner.set_over(tenant, over);
                    over
                }
                Err(e) => {
                    tracing::error!(%tenant, error = %e, "failed to count WASM invocation log");
                    return;
                }
            },
        };
        if over < WASM_INVOCATIONS_PRUNE_BATCH_ROWS {
            return;
        }
        match store.prune_wasm_invocations(tenant).await {
            Ok(()) => pruner.set_over(tenant, 0),
            Err(e) => tracing::error!(%tenant, error = %e, "failed to prune WASM invocation log"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tenant_is_looked_at_once_then_only_after_a_full_batch() {
        let pruner = WasmLogPruner::default();
        assert!(pruner.mark("a"), "an uncounted tenant starts the drain");
        assert_eq!(pruner.next().as_deref(), Some("a"));
        pruner.set_over("a", 0);
        assert_eq!(pruner.next(), None);

        for _ in 1..WASM_INVOCATIONS_PRUNE_BATCH_ROWS {
            assert!(!pruner.mark("a"), "inserts short of a batch cost nothing");
        }
        assert_eq!(pruner.next(), None, "nothing is waiting");
        assert!(pruner.mark("a"), "the batch-th insert wakes the pruner");
        assert_eq!(pruner.next().as_deref(), Some("a"));
    }

    #[test]
    fn a_log_already_a_batch_over_is_looked_at_after_counting() {
        let pruner = WasmLogPruner::default();
        pruner.mark("a");
        assert_eq!(pruner.next().as_deref(), Some("a"));
        pruner.set_over("a", 5_000);
        assert_eq!(pruner.next(), None);
        assert!(pruner.mark("a"), "a backlog is looked at on every insert");
        assert_eq!(pruner.next().as_deref(), Some("a"));
    }
}
