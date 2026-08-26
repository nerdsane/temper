//! Bounded safe scalar details.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize, de};

use crate::bounds::{MAX_DETAIL_ENTRIES, MAX_DETAILS_SERIALIZED_BYTES};
use crate::{BoundedDetailString, DetailKey, FailureContractError};

/// A safe scalar value in bounded failure details.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum FailureDetailValue {
    /// Bounded UTF-8 text.
    String(BoundedDetailString),
    /// Signed integer.
    Signed(i64),
    /// Unsigned integer.
    Unsigned(u64),
    /// Boolean flag.
    Bool(bool),
}

/// A deterministically ordered map of bounded, safe scalar details.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct BoundedFailureDetails(BTreeMap<DetailKey, FailureDetailValue>);

impl BoundedFailureDetails {
    /// Validate and construct a complete details map.
    pub fn new(
        values: BTreeMap<DetailKey, FailureDetailValue>,
    ) -> Result<Self, FailureContractError> {
        validate_details(&values)?;
        Ok(Self(values))
    }

    /// Borrow the ordered detail values.
    pub fn values(&self) -> &BTreeMap<DetailKey, FailureDetailValue> {
        &self.0
    }

    /// Insert one value while preserving every v1 budget.
    pub fn try_insert(
        &mut self,
        key: DetailKey,
        value: FailureDetailValue,
    ) -> Result<(), FailureContractError> {
        let previous = self.0.insert(key.clone(), value);
        if let Err(error) = validate_details(&self.0) {
            match previous {
                Some(previous) => {
                    self.0.insert(key, previous);
                }
                None => {
                    self.0.remove(&key);
                }
            }
            return Err(error);
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for BoundedFailureDetails {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let values = BTreeMap::<DetailKey, FailureDetailValue>::deserialize(deserializer)?;
        Self::new(values).map_err(de::Error::custom)
    }
}

fn validate_details(
    values: &BTreeMap<DetailKey, FailureDetailValue>,
) -> Result<(), FailureContractError> {
    if values.len() > MAX_DETAIL_ENTRIES {
        return Err(FailureContractError::TooManyDetails {
            max: MAX_DETAIL_ENTRIES,
            actual: values.len(),
        });
    }
    let encoded = serde_json::to_vec(values)
        .map_err(|error| FailureContractError::DetailsEncoding(error.to_string()))?;
    if encoded.len() > MAX_DETAILS_SERIALIZED_BYTES {
        return Err(FailureContractError::DetailsTooLarge {
            max: MAX_DETAILS_SERIALIZED_BYTES,
            actual: encoded.len(),
        });
    }
    Ok(())
}
