//! Versioned transport-neutral request and response contracts.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::DataOutcomeV1;
use super::{OrderV1, PageV1};

/// The only application-data ABI version understood by this SDK release.
pub const DATA_ABI_VERSION_V1: u32 = 1;

/// A JSON object used for entity values, patches, and action parameters.
pub type DataObject = Map<String, Value>;

/// One versioned request to the governed application-data host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DataRequestV1 {
    /// ABI version. Must be [`DATA_ABI_VERSION_V1`].
    pub abi: u32,
    /// Requested operation.
    pub operation: DataOperationV1,
}

impl DataRequestV1 {
    /// Construct a v1 request.
    pub const fn new(operation: DataOperationV1) -> Self {
        Self {
            abi: DATA_ABI_VERSION_V1,
            operation,
        }
    }
}

/// Operations supported by the v1 host ABI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DataOperationV1 {
    /// Read one entity, optionally requiring a minimum committed sequence.
    EntityGet {
        /// Fully qualified CSDL entity type.
        entity_type: String,
        /// Canonical entity identifier.
        entity_id: String,
        /// Minimum sequence that the returned entity must represent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at_least_sequence: Option<u64>,
    },
    /// Query a bounded collection through the closed v1 query language.
    EntityQuery {
        /// Fully qualified CSDL entity type.
        entity_type: String,
        /// Optional closed predicate.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filter: Option<FilterV1>,
        /// Stable ordered fields.
        #[serde(default)]
        order_by: Vec<OrderV1>,
        /// Bounded page request.
        page: PageV1,
    },
    /// Create one entity.
    EntityCreate {
        /// Fully qualified CSDL entity type.
        entity_type: String,
        /// Typed entity value encoded with canonical property names.
        value: DataObject,
    },
    /// Patch one entity using an optional exact-sequence precondition.
    EntityPatch {
        /// Fully qualified CSDL entity type.
        entity_type: String,
        /// Canonical entity identifier.
        entity_id: String,
        /// Sequence that must still be current.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_sequence: Option<u64>,
        /// Canonical property patch.
        value: DataObject,
    },
    /// Invoke one granted bound action.
    ActionInvoke {
        /// Fully qualified CSDL entity type.
        entity_type: String,
        /// Canonical entity identifier.
        entity_id: String,
        /// Canonical action name.
        action: String,
        /// Sequence that must still be current.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_sequence: Option<u64>,
        /// Canonical action parameters.
        params: DataObject,
    },
    /// Execute bounded non-atomic operations in request order.
    Batch {
        /// Non-nesting batch items.
        items: Vec<BatchItemV1>,
    },
    /// Invoke one declared atomic composite action.
    CompositeInvoke {
        /// Fully qualified CSDL entity type.
        entity_type: String,
        /// Canonical entity identifier.
        entity_id: String,
        /// Canonical composite action name.
        action: String,
        /// Sequence that must still be current.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_sequence: Option<u64>,
        /// Canonical action parameters.
        params: DataObject,
    },
    /// Open a bounded File content read stream.
    FileReadOpen {
        /// Canonical File identifier.
        file_id: String,
        /// Optional immutable FileVersion identifier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version_id: Option<String>,
    },
    /// Open a bounded File content write stream.
    FileWriteOpen {
        /// Canonical File identifier.
        file_id: String,
        /// Sequence that must still be current at commit.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_sequence: Option<u64>,
        /// Optional exact content length declaration.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content_length: Option<u64>,
        /// Optional expected SHA-256 content hash.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content_hash: Option<String>,
    },
    /// Durably commit an open File write stream.
    FileWriteCommit {
        /// Invocation-scoped write-stream handle.
        stream_handle: u32,
    },
    /// Consume and discard an open File stream.
    FileStreamAbort {
        /// Invocation-scoped File stream handle.
        stream_handle: u32,
    },
}

/// Non-nesting operations accepted inside an ordinary batch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum BatchItemV1 {
    /// Read one entity.
    EntityGet {
        /// Fully qualified CSDL entity type.
        entity_type: String,
        /// Canonical entity identifier.
        entity_id: String,
        /// Minimum visible sequence.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at_least_sequence: Option<u64>,
    },
    /// Create one entity.
    EntityCreate {
        /// Fully qualified CSDL entity type.
        entity_type: String,
        /// Canonical entity value.
        value: DataObject,
    },
    /// Patch one entity.
    EntityPatch {
        /// Fully qualified CSDL entity type.
        entity_type: String,
        /// Canonical entity identifier.
        entity_id: String,
        /// Sequence that must still be current.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_sequence: Option<u64>,
        /// Canonical property patch.
        value: DataObject,
    },
    /// Invoke one bound action.
    ActionInvoke {
        /// Fully qualified CSDL entity type.
        entity_type: String,
        /// Canonical entity identifier.
        entity_id: String,
        /// Canonical action name.
        action: String,
        /// Sequence that must still be current.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_sequence: Option<u64>,
        /// Canonical action parameters.
        params: DataObject,
    },
}

/// Closed v1 query predicate language.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FilterV1 {
    /// Compare one property with a typed scalar.
    Compare {
        /// Canonical property name.
        field: String,
        /// Closed comparison operator.
        operator: CompareOperatorV1,
        /// Typed comparison value.
        value: ScalarV1,
    },
    /// Test whether one property is null or absent.
    IsNull {
        /// Canonical property name.
        field: String,
        /// Whether null, rather than non-null, is required.
        is_null: bool,
    },
    /// Require every nested predicate.
    And {
        /// Non-empty bounded predicates.
        operands: Vec<FilterV1>,
    },
    /// Require at least one nested predicate.
    Or {
        /// Non-empty bounded predicates.
        operands: Vec<FilterV1>,
    },
    /// Negate one predicate.
    Not {
        /// Predicate to negate.
        operand: Box<FilterV1>,
    },
}

/// Operators supported by scalar comparisons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompareOperatorV1 {
    /// Equal.
    Eq,
    /// Not equal.
    Ne,
    /// Less than.
    Lt,
    /// Less than or equal.
    Le,
    /// Greater than.
    Gt,
    /// Greater than or equal.
    Ge,
}

/// Closed scalar values accepted by the v1 query language.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ScalarV1 {
    /// Boolean value.
    Boolean(bool),
    /// Signed 64-bit integer value.
    Int64(i64),
    /// Finite IEEE-754 double value.
    Double(f64),
    /// UTF-8 string value.
    String(String),
    /// Canonical hyphenated UUID value.
    Guid(String),
    /// RFC-3339 instant value.
    DateTimeOffset(String),
    /// Canonical arbitrary-precision decimal text.
    Decimal(String),
    /// Declared CSDL enum member.
    Enum(EnumValueV1),
}

/// A canonical CSDL enum member.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnumValueV1 {
    /// Fully qualified CSDL enum type.
    pub type_name: String,
    /// Canonical enum member name.
    pub member: String,
}

/// Successful v1 operation results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DataResultV1 {
    /// One authoritative entity value.
    Entity {
        /// Canonical entity value.
        value: DataObject,
        /// Entity stream sequence represented by the value.
        sequence: u64,
    },
    /// One bounded ordered collection page.
    Page {
        /// Values and their per-entity sequences.
        values: Vec<SequencedValueV1>,
        /// Opaque continuation cursor when more candidates remain.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        next_cursor: Option<String>,
    },
    /// A committed create or patch.
    Write {
        /// Durable per-entity commit token.
        commit: CommitToken,
        /// Returned entity value when it fits the response budget.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<DataObject>,
        /// Whether the value was omitted after the write committed.
        value_omitted: bool,
    },
    /// A committed action.
    Action {
        /// Durable per-entity commit token.
        commit: CommitToken,
        /// Action result when present and within budget.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
        /// Whether the result was omitted after the action committed.
        result_omitted: bool,
    },
    /// Request-ordered outcomes from a non-atomic batch.
    Batch {
        /// One independent outcome per input item.
        outcomes: Vec<DataOutcomeV1>,
    },
    /// An open bounded File read stream.
    FileRead {
        /// Invocation-scoped read-stream handle.
        stream_handle: u32,
        /// File metadata resolved at open time.
        metadata: FileMetadataV1,
        /// File entity sequence resolved at open time.
        sequence: u64,
        /// Content length when known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content_length: Option<u64>,
    },
    /// An open bounded File write stream.
    FileWrite {
        /// Invocation-scoped write-stream handle.
        stream_handle: u32,
    },
    /// A durably committed File content write.
    FileCommitted {
        /// Durable File commit token.
        commit: CommitToken,
        /// Updated File metadata when available.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<FileMetadataV1>,
    },
    /// An explicitly aborted File stream.
    Aborted,
}

/// Entity value paired with the sequence it represents.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SequencedValueV1 {
    /// Canonical entity value.
    pub value: DataObject,
    /// Entity stream sequence represented by the value.
    pub sequence: u64,
}

/// Per-entity post-commit consistency token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitToken {
    /// Fully qualified entity type.
    pub entity_type: String,
    /// Canonical entity identifier.
    pub entity_id: String,
    /// Durable entity stream sequence.
    pub sequence: u64,
}

/// File metadata returned independently from File content bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileMetadataV1 {
    /// Canonical File identifier.
    pub file_id: String,
    /// Immutable version identifier when reading a version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
    /// Declared MIME type when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    /// Canonical content hash when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
}

/// Structured error returned before any HTTP mapping.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModuleDataError {
    /// Closed low-cardinality category.
    pub kind: ModuleDataErrorKind,
    /// Stable machine-readable code.
    pub code: String,
    /// Bounded safe explanation.
    pub message: String,
    /// Stable retry guidance.
    pub retryability: Retryability,
    /// Optional governance decision identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_id: Option<String>,
    /// Optional bounded typed metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Box<DataObject>>,
}

impl ModuleDataError {
    /// Construct a stable structured error.
    pub fn new(
        kind: ModuleDataErrorKind,
        code: impl Into<String>,
        message: impl Into<String>,
        retryability: Retryability,
    ) -> Self {
        Self {
            kind,
            code: code.into(),
            message: message.into(),
            retryability,
            decision_id: None,
            details: None,
        }
    }
}

impl core::fmt::Display for ModuleDataError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ModuleDataError {}

/// Closed v1 error taxonomy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModuleDataErrorKind {
    /// Malformed or unsupported caller input.
    InvalidRequest,
    /// Input or binding differs from the verified schema.
    SchemaMismatch,
    /// Requested resource does not exist.
    NotFound,
    /// Resource already exists.
    AlreadyExists,
    /// Capability or Cedar authorization denied the operation.
    AuthorizationDenied,
    /// An IOA guard rejected the action.
    GuardRejected,
    /// A declared relation rejected the mutation.
    RelationViolation,
    /// Verification state does not permit the operation.
    VerificationFailed,
    /// Exact-sequence or other concurrency conflict.
    Conflict,
    /// Requested committed state cannot be observed within the bounded path.
    ConsistencyUnavailable,
    /// A declared operation or byte budget was exhausted.
    BudgetExceeded,
    /// A transient dependency is unavailable.
    TransientUnavailable,
    /// Safe internal failure without sensitive details.
    Internal,
}

/// Whether retrying an operation can be useful.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Retryability {
    /// Repeating the same request cannot help.
    Never,
    /// Retry only after refreshing state or a commit token.
    AfterRefresh,
    /// Retry with bounded exponential backoff.
    WithBackoff,
}

#[cfg(test)]
#[path = "contracts_tests.rs"]
mod tests;
