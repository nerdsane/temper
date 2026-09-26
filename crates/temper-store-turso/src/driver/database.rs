//! Keep native database internals out of every store user's auto-trait proof.

use std::fmt::Debug;
use std::future::Future;

use super::{Connection, DriverError, DriverHandle};

trait DatabaseDriver: Debug + Send + Sync {
    fn connect(&self) -> Result<Connection, DriverError>;
}

impl DatabaseDriver for turso::Database {
    fn connect(&self) -> Result<Connection, DriverError> {
        let conn = self.connect()?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(Connection::Local(DriverHandle::new(conn)))
    }
}

impl DatabaseDriver for turso_serverless::Database {
    fn connect(&self) -> Result<Connection, DriverError> {
        Ok(Connection::Remote(self.connect()?))
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

    pub(crate) fn connect(&self) -> Result<Connection, DriverError> {
        self.driver.connect()
    }
}
