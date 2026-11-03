//! Idle connections of one synced database.

use std::sync::Arc;

use crate::driver::{DriverHandle, SharedConnection};

/// Most idle connections kept for reuse.
const MAX_IDLE_CONNECTIONS: usize = 256;

type IdleConnection = Arc<DriverHandle<'static, turso::Connection>>;

/// Idle connections of one synced database.
#[derive(Clone, Default)]
pub(crate) struct ConnectionPool(Arc<std::sync::Mutex<Vec<IdleConnection>>>);

impl ConnectionPool {
    pub(super) fn take(&self) -> Option<IdleConnection> {
        self.0.lock().unwrap().pop()
    }

    /// Keep the connection under `lease` for reuse unless a result stream or
    /// transaction still holds the lease or it was left inside a transaction.
    pub(crate) fn give_back(&self, lease: &SharedConnection, conn: &IdleConnection) {
        if Arc::strong_count(lease) != 1 || !conn.is_autocommit().unwrap_or(false) {
            return;
        }
        let mut idle = self.0.lock().unwrap();
        if idle.len() < MAX_IDLE_CONNECTIONS {
            idle.push(conn.clone());
        }
    }
}
