use serde_json::Value;

use super::{CommonsStorageCapError, read_i64, read_string};

#[derive(Debug, Clone)]
pub(super) struct OwnerStorageProfile {
    pub(super) owner_id: String,
    pub(super) cap_bytes: i64,
    pub(super) suspended: bool,
}

impl OwnerStorageProfile {
    pub(super) fn from_fields(
        entity_id: &str,
        fields: &Value,
    ) -> Result<Self, CommonsStorageCapError> {
        let owner_id = read_string(fields, "AccountId")
            .or_else(|| read_string(fields, "Id"))
            .unwrap_or_else(|| entity_id.to_string());
        let Some(cap_bytes) = read_i64(fields, "StorageCapBytes") else {
            return Ok(Self {
                owner_id,
                cap_bytes: i64::MAX,
                suspended: false,
            });
        };
        let suspended = fields
            .get("Status")
            .and_then(|value| value.as_str())
            .is_some_and(|status| status == "Suspended");
        Ok(Self {
            owner_id,
            cap_bytes: cap_bytes.max(0),
            suspended,
        })
    }
}
