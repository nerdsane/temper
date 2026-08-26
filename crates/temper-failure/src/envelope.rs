//! V1 envelope and closed classification types.

use serde::{Deserialize, Deserializer, Serialize, de};

use crate::bounds::{FAILURE_ENVELOPE_VERSION_V1, MAX_OPERATION_ATTEMPT};
use crate::{
    BoundedDiagnostic, BoundedFailureDetails, DetailKey, FailureContractError, FailureDetailValue,
    OperationId, OperationKind, ProvenanceToken, StableFailureCode,
};

/// Closed, low-cardinality application failure categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCategory {
    /// A dependency or capacity condition may clear.
    Transient,
    /// Input, schema, state, relation, or concurrency integrity failed.
    Integrity,
    /// Cedar or capability authorization denied the operation.
    Authorization,
    /// A declared resource or delivery budget was exhausted.
    Budget,
    /// Whether the causal operation applied is not known.
    Ambiguous,
    /// Repeating the same operation cannot resolve the failure.
    Permanent,
}

/// Stable guidance for a later, separately governed operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureRetryability {
    /// Repeating the same operation cannot help.
    Never,
    /// Retry only after refreshing committed state.
    AfterRefresh,
    /// Retry through a new operation with bounded backoff.
    WithBackoff,
    /// Retry only after authorization context changes.
    AfterAuthorization,
    /// Reconcile the outcome before deciding whether to issue new work.
    Reconcile,
}

/// Whether the causal operation is known to have applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureOutcome {
    /// The operation is known not to have applied.
    NotApplied,
    /// The operation applied but a later stage failed.
    Applied,
    /// The kernel cannot prove whether the operation applied.
    Unknown,
}

/// Closed provenance sources for v1 adapters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureSource {
    /// Typed module-data service or host ABI.
    ModuleData,
    /// Cross-entity reaction delivery.
    Reaction,
    /// Actor, integration, or durable state timeout.
    Timeout,
    /// Cedar or capability authorization.
    Authorization,
    /// External transport or provider operation.
    ExternalOperation,
    /// WASM engine or guest invocation.
    Wasm,
    /// Explicit compatibility adaptation of a free-form legacy failure.
    Legacy,
}

/// A bounded operation attempt number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct OperationAttempt(u16);

impl OperationAttempt {
    /// Validate and construct an operation attempt.
    pub fn new(value: u16) -> Result<Self, FailureContractError> {
        if value > MAX_OPERATION_ATTEMPT {
            return Err(FailureContractError::AttemptOutOfRange {
                max: MAX_OPERATION_ATTEMPT,
                actual: value,
            });
        }
        Ok(Self(value))
    }

    /// Return the attempt number.
    pub const fn get(self) -> u16 {
        self.0
    }
}

impl<'de> Deserialize<'de> for OperationAttempt {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(u16::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// Stable causal operation identity carried across adapters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CausalOperationV1 {
    /// Stable operation identifier.
    pub id: OperationId,
    /// Stable operation kind.
    pub kind: OperationKind,
    /// Bounded attempt within the owning delivery mechanism.
    pub attempt: OperationAttempt,
    /// Optional parent operation identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<OperationId>,
}

/// Typed source provenance for a failure adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FailureProvenanceV1 {
    /// Closed source boundary.
    pub source: FailureSource,
    /// Kernel component that performed the adaptation.
    pub component: ProvenanceToken,
    /// Optional stable source-specific code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_code: Option<ProvenanceToken>,
}

/// Canonical bounded application-facing failure envelope, version 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureEnvelopeV1 {
    /// Wire-contract version; always one for this type.
    pub version: u16,
    /// Closed routing category.
    pub category: FailureCategory,
    /// Stable machine-readable code.
    pub code: StableFailureCode,
    /// Guidance for a later, separately governed operation.
    pub retryability: FailureRetryability,
    /// Whether the causal operation applied.
    pub outcome: FailureOutcome,
    /// Causal operation identity.
    pub operation: CausalOperationV1,
    /// Source adapter provenance.
    pub provenance: FailureProvenanceV1,
    /// Optional diagnostic text, never used for routing.
    pub message: Option<BoundedDiagnostic>,
    /// Whether an upstream diagnostic exceeded the v1 safety budget.
    pub diagnostic_omitted: bool,
    /// Bounded safe scalar details.
    pub details: BoundedFailureDetails,
    /// Whether upstream details were omitted as a complete field.
    pub details_omitted: bool,
}

impl Serialize for FailureEnvelopeV1 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        validate_envelope(self).map_err(serde::ser::Error::custom)?;

        #[derive(Serialize)]
        struct WireEnvelope<'a> {
            version: u16,
            category: FailureCategory,
            code: &'a StableFailureCode,
            retryability: FailureRetryability,
            outcome: FailureOutcome,
            operation: &'a CausalOperationV1,
            provenance: &'a FailureProvenanceV1,
            #[serde(skip_serializing_if = "Option::is_none")]
            message: &'a Option<BoundedDiagnostic>,
            diagnostic_omitted: bool,
            details: &'a BoundedFailureDetails,
            details_omitted: bool,
        }

        WireEnvelope {
            version: self.version,
            category: self.category,
            code: &self.code,
            retryability: self.retryability,
            outcome: self.outcome,
            operation: &self.operation,
            provenance: &self.provenance,
            message: &self.message,
            diagnostic_omitted: self.diagnostic_omitted,
            details: &self.details,
            details_omitted: self.details_omitted,
        }
        .serialize(serializer)
    }
}

impl FailureEnvelopeV1 {
    /// Construct a validated v1 envelope without optional diagnostics or details.
    pub fn new(
        category: FailureCategory,
        code: StableFailureCode,
        retryability: FailureRetryability,
        outcome: FailureOutcome,
        operation: CausalOperationV1,
        provenance: FailureProvenanceV1,
    ) -> Result<Self, FailureContractError> {
        validate_semantics(category, retryability, outcome)?;
        Ok(Self {
            version: FAILURE_ENVELOPE_VERSION_V1,
            category,
            code,
            retryability,
            outcome,
            operation,
            provenance,
            message: None,
            diagnostic_omitted: false,
            details: BoundedFailureDetails::default(),
            details_omitted: false,
        })
    }

    /// Attach a diagnostic or mark the complete diagnostic omitted when oversized.
    pub fn with_diagnostic(mut self, message: impl Into<String>) -> Self {
        match BoundedDiagnostic::new(message) {
            Ok(message) => {
                self.message = Some(message);
                self.diagnostic_omitted = false;
            }
            Err(_) => {
                self.message = None;
                self.diagnostic_omitted = true;
            }
        }
        self
    }

    /// Insert one safe detail, omitting the complete map if any budget is exceeded.
    pub fn insert_detail_or_omit(&mut self, key: DetailKey, value: FailureDetailValue) {
        if self.details_omitted {
            return;
        }
        if self.details.try_insert(key, value).is_err() {
            self.details = BoundedFailureDetails::default();
            self.details_omitted = true;
        }
    }
}

impl<'de> Deserialize<'de> for FailureEnvelopeV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireEnvelope {
            version: u16,
            category: FailureCategory,
            code: StableFailureCode,
            retryability: FailureRetryability,
            outcome: FailureOutcome,
            operation: CausalOperationV1,
            provenance: FailureProvenanceV1,
            #[serde(default)]
            message: Option<BoundedDiagnostic>,
            diagnostic_omitted: bool,
            details: BoundedFailureDetails,
            details_omitted: bool,
        }

        let wire = WireEnvelope::deserialize(deserializer)?;
        let envelope = Self {
            version: wire.version,
            category: wire.category,
            code: wire.code,
            retryability: wire.retryability,
            outcome: wire.outcome,
            operation: wire.operation,
            provenance: wire.provenance,
            message: wire.message,
            diagnostic_omitted: wire.diagnostic_omitted,
            details: wire.details,
            details_omitted: wire.details_omitted,
        };
        validate_envelope(&envelope).map_err(de::Error::custom)?;
        Ok(envelope)
    }
}

fn validate_envelope(envelope: &FailureEnvelopeV1) -> Result<(), FailureContractError> {
    if envelope.version != FAILURE_ENVELOPE_VERSION_V1 {
        return Err(FailureContractError::UnsupportedVersion {
            expected: FAILURE_ENVELOPE_VERSION_V1,
            actual: envelope.version,
        });
    }
    validate_semantics(envelope.category, envelope.retryability, envelope.outcome)?;
    if envelope.diagnostic_omitted && envelope.message.is_some() {
        return Err(FailureContractError::ContradictoryOmission { field: "message" });
    }
    if envelope.details_omitted && !envelope.details.values().is_empty() {
        return Err(FailureContractError::ContradictoryOmission { field: "details" });
    }
    Ok(())
}

fn validate_semantics(
    category: FailureCategory,
    retryability: FailureRetryability,
    outcome: FailureOutcome,
) -> Result<(), FailureContractError> {
    if category == FailureCategory::Ambiguous && outcome != FailureOutcome::Unknown {
        return Err(FailureContractError::AmbiguousCategoryWithKnownOutcome);
    }
    if (outcome == FailureOutcome::Unknown) != (retryability == FailureRetryability::Reconcile) {
        return Err(FailureContractError::InvalidReconciliationGuidance);
    }
    Ok(())
}
