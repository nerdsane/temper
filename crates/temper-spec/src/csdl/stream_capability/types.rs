//! Canonical verified stream capability values.

use serde::{Deserialize, Serialize};

use super::VerifiedStreamMigrationProvenanceV1;

/// A stable schema-verification failure for the stream vocabulary.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StreamCapabilityError {
    /// An entity type occurred more than once in the document.
    #[error("duplicate CSDL entity type '{0}'")]
    DuplicateEntityType(String),
    /// A closed stream annotation occurred more than once.
    #[error("entity '{entity_type}' has duplicate annotation '{term}'")]
    DuplicateAnnotation {
        /// Fully qualified entity type.
        entity_type: String,
        /// Exact vocabulary term.
        term: &'static str,
    },
    /// A stream entity omitted its required mutability declaration.
    #[error("stream entity '{0}' is missing Temper.Vocab.Stream.Mutability")]
    MissingMutability(String),
    /// A closed annotation used the wrong CSDL value kind.
    #[error("entity '{entity_type}' annotation '{term}' requires {expected}")]
    InvalidAnnotationValue {
        /// Fully qualified entity type.
        entity_type: String,
        /// Exact vocabulary term.
        term: &'static str,
        /// Required CSDL value kind.
        expected: &'static str,
    },
    /// Mutability contained a value outside the closed vocabulary.
    #[error("entity '{entity_type}' has unknown stream mutability '{value}'")]
    UnknownMutability {
        /// Fully qualified entity type.
        entity_type: String,
        /// Rejected value.
        value: String,
    },
    /// A mutable direct stream did not expose OData `$value`.
    #[error("mutable stream entity '{0}' must declare HasStream=true")]
    MutableWithoutHasStream(String),
    /// Only one half of a version contract was declared.
    #[error("entity '{0}' must declare both VersionEntityType and VersionCollection")]
    IncompleteVersionContract(String),
    /// An immutable stream omitted its parent navigation.
    #[error("immutable stream entity '{0}' must declare AuthorizationParent")]
    MissingAuthorizationParent(String),
    /// A declaration is forbidden for the chosen mutability.
    #[error("entity '{entity_type}' annotation '{term}' is incompatible with its mutability")]
    IncompatibleAnnotation {
        /// Fully qualified entity type.
        entity_type: String,
        /// Exact vocabulary term.
        term: &'static str,
    },
    /// A referenced type is absent.
    #[error("entity '{entity_type}' references unknown stream type '{target_type}'")]
    UnknownTargetType {
        /// Declaring entity type.
        entity_type: String,
        /// Missing fully qualified type.
        target_type: String,
    },
    /// A named navigation is absent or ambiguous.
    #[error("entity '{entity_type}' does not have exactly one navigation '{navigation}'")]
    InvalidNavigation {
        /// Declaring entity type.
        entity_type: String,
        /// Canonical navigation name.
        navigation: String,
    },
    /// A navigation target or collection shape is incorrect.
    #[error(
        "entity '{entity_type}' navigation '{navigation}' must target '{expected_target}' with collection={expected_collection}"
    )]
    NavigationTargetMismatch {
        /// Declaring entity type.
        entity_type: String,
        /// Parent navigation name.
        navigation: String,
        /// Required fully qualified target.
        expected_target: String,
        /// Required collection shape.
        expected_collection: bool,
    },
    /// Parent ownership could not be proven from referential constraints.
    #[error(
        "entity '{entity_type}' navigation '{navigation}' has invalid parent referential constraints"
    )]
    InvalidReferentialConstraint {
        /// Immutable child type.
        entity_type: String,
        /// Parent navigation name.
        navigation: String,
    },
    /// The declared version and parent navigation are not mutual.
    #[error("stream version contract for '{0}' is not mutual")]
    NonMutualVersionContract(String),
    /// An activation marker selected an unsupported descriptor contract.
    #[error("entity '{entity_type}' activates unsupported stream descriptor contract {version}")]
    UnsupportedDescriptorContract {
        /// Fully qualified entity type.
        entity_type: String,
        /// Unsupported contract version.
        version: i64,
    },
    /// An activated descriptor contract omitted its historical migration mapping.
    #[error("stream entity '{0}' activates descriptors without migration provenance")]
    MissingMigrationProvenance(String),
    /// A migration provenance declaration omitted one required annotation.
    #[error("entity '{entity_type}' migration provenance is missing '{term}'")]
    IncompleteMigrationProvenance {
        /// Fully qualified entity type.
        entity_type: String,
        /// Missing closed annotation term.
        term: &'static str,
    },
    /// Parent-authorized provenance did not declare exactly one parent parameter.
    #[error("entity '{entity_type}' migration parent binding requires_parent={requires_parent}")]
    MigrationParentBinding {
        /// Fully qualified entity type.
        entity_type: String,
        /// Whether the stream capability requires a parent.
        requires_parent: bool,
    },
    /// A migration selected an unsupported storage reconstruction contract.
    #[error("entity '{entity_type}' selects unsupported migration storage contract {version}")]
    UnsupportedMigrationStorageContract {
        /// Fully qualified entity type.
        entity_type: String,
        /// Unsupported contract version.
        version: i64,
    },
    /// A storage-key prefix was unsafe or exceeded its byte budget.
    #[error("entity '{0}' has an invalid migration storage-key prefix")]
    InvalidStorageKeyPrefix(String),
}

/// Closed stream replacement semantics emitted by CSDL verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamCapabilityMutabilityV1 {
    /// A later verified content commit may replace the descriptor.
    Mutable,
    /// The first descriptor is permanent for the subject.
    Immutable,
}

/// Canonical stream semantics proven from CSDL navigation metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifiedStreamCapabilityV1 {
    /// Fully qualified stream subject type.
    pub subject_type: String,
    /// Verified replacement semantics.
    pub mutability: StreamCapabilityMutabilityV1,
    /// Fully qualified immutable version type, when declared by a mutable stream.
    pub version_entity_type: Option<String>,
    /// Canonical collection navigation from the mutable subject to its versions.
    pub version_collection_navigation: Option<String>,
    /// Canonical navigation from an immutable subject to its authorization parent.
    pub authorization_parent_navigation: Option<String>,
    /// Fully qualified authorization-parent type.
    pub authorization_parent_type: Option<String>,
    /// Verified interpretation of historical publication events, when declared.
    pub migration_provenance: Option<VerifiedStreamMigrationProvenanceV1>,
    /// Whether this schema version activates strict descriptor contract V1.
    pub descriptor_contract_v1_active: bool,
}
