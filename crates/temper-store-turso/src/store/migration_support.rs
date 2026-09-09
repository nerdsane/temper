use temper_runtime::persistence::{PersistenceError, storage_error};

use super::instrumentation::InstrumentedConnection;

/// Execute a SQLite `ADD COLUMN` migration while rejecting every error except
/// the duplicate-column result expected on an already-migrated database.
pub(super) async fn add_column_if_missing(
    connection: &InstrumentedConnection,
    statement: &str,
) -> Result<(), PersistenceError> {
    if let Err(error) = connection.execute(statement, ()).await {
        let message = error.to_string().to_ascii_lowercase();
        if !message.contains("duplicate column")
            && !message.contains("already exists")
            && !message.contains("already has")
        {
            return Err(storage_error(error));
        }
    }
    Ok(())
}
