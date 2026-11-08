//! Keep native database internals out of every store user's auto-trait proof.

use std::fmt::Debug;
use std::future::Future;
use std::pin::Pin;

use super::{Connection, DriverError, DriverHandle, Lease, SyncCopy};

type ConnectFuture<'a> = Pin<Box<dyn Future<Output = Result<Connection, DriverError>> + Send + 'a>>;

trait DatabaseDriver: Debug + Send + Sync {
    fn connect(&self) -> ConnectFuture<'_>;
}

fn local_connection(conn: turso::Connection) -> Result<Connection, DriverError> {
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(Connection::Local {
        conn: Lease::new(std::sync::Arc::new(DriverHandle::new(conn)), None),
        pool: None,
    })
}

impl DatabaseDriver for turso::Database {
    fn connect(&self) -> ConnectFuture<'_> {
        let conn = self.connect().map_err(DriverError::from);
        Box::pin(std::future::ready(conn.and_then(local_connection)))
    }
}

impl DatabaseDriver for turso_serverless::Database {
    fn connect(&self) -> ConnectFuture<'_> {
        let conn = self.connect().map(Connection::Remote);
        Box::pin(std::future::ready(conn.map_err(DriverError::from)))
    }
}

impl DatabaseDriver for SyncCopy {
    fn connect(&self) -> ConnectFuture<'_> {
        Box::pin(SyncCopy::connect(self))
    }
}

/// One erased driver per database, shared by `TursoEventStore`'s existing Arc.
///
/// A concrete embedded database exposes a large graph of engine types to Rust's
/// structural Send/Sync checking, including in crates which only use the store.
/// This boundary proves those bounds here once, without unsafe implementations
/// or erasing returned futures.
#[derive(Debug)]
pub(crate) struct Database {
    driver: Box<dyn DatabaseDriver>,
}

impl Database {
    pub(crate) fn local(path: &str) -> impl Future<Output = Result<Self, DriverError>> + Send {
        async move {
            Ok(Self {
                driver: Box::new(turso::Builder::new_local(path).build().await?),
            })
        }
    }

    pub(crate) fn remote(
        url: &str,
        token: &str,
    ) -> impl Future<Output = Result<Self, DriverError>> + Send {
        async move {
            Ok(Self {
                driver: Box::new(
                    turso_serverless::Builder::new_remote(url)
                        .with_auth_token(token)
                        .build()
                        .await?,
                ),
            })
        }
    }

    /// A local file read and written through `io`, which a test can count.
    #[cfg(test)]
    pub(crate) fn local_with_io(
        path: &str,
        io: std::sync::Arc<dyn turso::core::IO>,
    ) -> impl Future<Output = Result<Self, DriverError>> + Send {
        async move {
            Ok(Self {
                driver: Box::new(
                    turso::Builder::new_local(path)
                        .with_io_impl(io)
                        .build()
                        .await?,
                ),
            })
        }
    }

    /// A local file whose commits are copied to its own Turso Cloud database.
    pub(crate) fn synced(copy: SyncCopy) -> Self {
        Self {
            driver: Box::new(copy),
        }
    }

    pub(crate) fn connect(&self) -> impl Future<Output = Result<Connection, DriverError>> + Send {
        self.driver.connect()
    }
}
