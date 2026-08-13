use serde::{Deserialize, Serialize};

use super::{DATA_ABI_VERSION_V1, DataResultV1, ModuleDataError};

/// One versioned response from the governed application-data host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DataResponseV1 {
    /// ABI version. Always [`DATA_ABI_VERSION_V1`].
    pub abi: u32,
    /// Successful result or structured error.
    pub outcome: DataOutcomeV1,
}

impl DataResponseV1 {
    /// Construct a successful v1 response.
    pub const fn ok(result: DataResultV1) -> Self {
        Self {
            abi: DATA_ABI_VERSION_V1,
            outcome: DataOutcomeV1::Ok { result },
        }
    }

    /// Construct an error v1 response.
    pub const fn error(error: ModuleDataError) -> Self {
        Self {
            abi: DATA_ABI_VERSION_V1,
            outcome: DataOutcomeV1::Error { error },
        }
    }
}

/// Success or structured domain error.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DataOutcomeV1 {
    /// Successful operation outcome.
    Ok { result: DataResultV1 },
    /// Rejected or failed operation outcome.
    Error { error: ModuleDataError },
}
