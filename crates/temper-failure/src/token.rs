//! Bounded string primitives used by the v1 envelope.

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::FailureContractError;
use crate::bounds::{
    MAX_DETAIL_KEY_BYTES, MAX_DETAIL_STRING_BYTES, MAX_DIAGNOSTIC_BYTES, MAX_FAILURE_CODE_BYTES,
    MAX_OPERATION_ID_BYTES, MAX_OPERATION_KIND_BYTES, MAX_PROVENANCE_TOKEN_BYTES,
};

macro_rules! bounded_token {
    ($name:ident, $doc:literal, $field:literal, $max:expr) => {
        #[doc = $doc]
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            #[doc = concat!("Validate and construct a `", stringify!($name), "`.")]
            pub fn new(value: impl Into<String>) -> Result<Self, FailureContractError> {
                let value = value.into();
                validate_token($field, &value, $max)?;
                Ok(Self(value))
            }

            /// Borrow the validated token.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(de::Error::custom)
            }
        }
    };
}

macro_rules! bounded_text {
    ($name:ident, $doc:literal, $field:literal, $max:expr) => {
        #[doc = $doc]
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            #[doc = concat!("Validate and construct a `", stringify!($name), "`.")]
            pub fn new(value: impl Into<String>) -> Result<Self, FailureContractError> {
                let value = value.into();
                validate_text($field, &value, $max)?;
                Ok(Self(value))
            }

            /// Borrow the validated text.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(de::Error::custom)
            }
        }
    };
}

bounded_token!(
    StableFailureCode,
    "A stable, bounded machine-readable failure code.",
    "failure code",
    MAX_FAILURE_CODE_BYTES
);
bounded_token!(
    OperationId,
    "A bounded causal operation identifier.",
    "operation id",
    MAX_OPERATION_ID_BYTES
);
bounded_token!(
    OperationKind,
    "A bounded machine-readable operation kind.",
    "operation kind",
    MAX_OPERATION_KIND_BYTES
);
bounded_token!(
    ProvenanceToken,
    "A bounded component or source-code provenance token.",
    "provenance token",
    MAX_PROVENANCE_TOKEN_BYTES
);
bounded_token!(
    DetailKey,
    "A bounded safe-detail key.",
    "detail key",
    MAX_DETAIL_KEY_BYTES
);
bounded_text!(
    BoundedDiagnostic,
    "A bounded human-readable diagnostic that is never a routing input.",
    "diagnostic",
    MAX_DIAGNOSTIC_BYTES
);
bounded_text!(
    BoundedDetailString,
    "A bounded string value in safe failure details.",
    "detail string",
    MAX_DETAIL_STRING_BYTES
);

fn validate_token(
    field: &'static str,
    value: &str,
    max: usize,
) -> Result<(), FailureContractError> {
    if value.is_empty() {
        return Err(FailureContractError::EmptyToken { field });
    }
    validate_text(field, value, max)?;
    if let Some(index) = value.bytes().position(|byte| {
        !byte.is_ascii_alphanumeric() && !matches!(byte, b'.' | b'_' | b':' | b'-')
    }) {
        return Err(FailureContractError::InvalidTokenByte { field, index });
    }
    Ok(())
}

fn validate_text(field: &'static str, value: &str, max: usize) -> Result<(), FailureContractError> {
    if value.len() > max {
        return Err(FailureContractError::FieldTooLong {
            field,
            max,
            actual: value.len(),
        });
    }
    Ok(())
}
