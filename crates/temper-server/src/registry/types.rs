//! Type definitions for the specification registry.

use std::collections::BTreeMap;
use std::sync::Arc;

use temper_jit::swap::SwapController;
use temper_jit::table::TransitionTable;
use temper_spec::automaton::{Automaton, Integration, Webhook};
use temper_spec::cross_invariant::{CrossInvariantSpec, DeletePolicy};
use temper_spec::csdl::CsdlDocument;

use crate::trigger::types::ReactionRule;

/// Verification status for a single entity type.
#[derive(Debug, Clone, serde::Serialize)]
pub enum VerificationStatus {
    /// Verification has not started yet.
    Pending,
    /// Verification is currently running.
    Running,
    /// Verification completed with full cascade results.
    Completed(EntityVerificationResult),
    /// Restored from persistent storage without full verification results.
    ///
    /// The `all_passed` flag reflects the persisted status but the detailed
    /// level results may be synthetic summaries rather than actual cascade output.
    Restored(EntityVerificationResult),
}

impl VerificationStatus {
    /// Returns true if verification completed and all levels passed.
    pub fn is_passed(&self) -> bool {
        match self {
            Self::Completed(r) | Self::Restored(r) => r.all_passed,
            _ => false,
        }
    }
}

/// Summary of verification results for an entity type.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EntityVerificationResult {
    /// Whether all levels passed.
    pub all_passed: bool,
    /// Per-level summaries.
    pub levels: Vec<EntityLevelSummary>,
    /// Non-fatal disclosures, including runtime-only enforcement contracts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// Structured capability errors that blocked backend verification.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<VerificationCapabilityError>,
    /// ISO-8601 timestamp when verification completed.
    pub verified_at: String,
}

impl EntityVerificationResult {
    /// Preserve the complete cascade result for registry, persistence, and UI consumers.
    #[cfg(feature = "observe")]
    pub fn from_cascade(result: &temper_verify::CascadeResult, verified_at: String) -> Self {
        let levels = result
            .levels
            .iter()
            .map(|level| {
                let mut details = Vec::new();
                if !level.passed {
                    if let Some(simulation) = &level.simulation {
                        for violation in &simulation.liveness_violations {
                            details.push(VerificationDetail {
                                kind: "liveness_violation".into(),
                                property: violation.property.clone(),
                                description: violation.description.clone(),
                                actor_id: Some(violation.actor_id.clone()),
                            });
                        }
                        for violation in &simulation.violations {
                            details.push(VerificationDetail {
                                kind: "invariant_violation".into(),
                                property: violation.invariant.clone(),
                                description: format!(
                                    "Actor {} violated invariant at tick {} during action {}",
                                    violation.actor_id, violation.tick, violation.action
                                ),
                                actor_id: Some(violation.actor_id.clone()),
                            });
                        }
                    }
                    if let Some(verification) = &level.verification {
                        for counterexample in &verification.counterexamples {
                            details.push(VerificationDetail {
                                kind: "counterexample".into(),
                                property: counterexample.property.clone(),
                                description: format!(
                                    "Counterexample found with {} step trace",
                                    counterexample.trace.len()
                                ),
                                actor_id: None,
                            });
                        }
                        for transition in &verification.dead_transitions {
                            details.push(VerificationDetail {
                                kind: "dead_transition".into(),
                                property: transition.clone(),
                                description: "Transition is unreachable in model check".into(),
                                actor_id: None,
                            });
                        }
                    }
                    if let Some(prop_test) = &level.prop_test
                        && let Some(failure) = &prop_test.failure
                    {
                        details.push(VerificationDetail {
                            kind: "proptest_failure".into(),
                            property: failure.invariant.clone(),
                            description: format!(
                                "Property test failed after sequence: {}",
                                failure.action_sequence.join(" -> ")
                            ),
                            actor_id: None,
                        });
                    }
                }

                EntityLevelSummary {
                    level: level.level.to_string(),
                    passed: level.passed,
                    summary: level.summary.clone(),
                    details: (!details.is_empty()).then_some(details),
                }
            })
            .collect();

        Self {
            all_passed: result.all_passed,
            levels,
            warnings: result.warnings.clone(),
            errors: result
                .errors
                .iter()
                .map(|error| VerificationCapabilityError {
                    code: error.code.clone(),
                    invariant: error.invariant.clone(),
                    assertion: error.assertion.clone(),
                    source_span: error.source_span,
                })
                .collect(),
            verified_at,
        }
    }
}

/// Structured safety-capability error retained in deployment status records.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VerificationCapabilityError {
    /// Stable machine-readable diagnostic code.
    pub code: String,
    /// Declared invariant name.
    pub invariant: String,
    /// Unsupported assertion expression.
    pub assertion: String,
    /// Exact half-open assertion source span.
    pub source_span: temper_spec::automaton::SourceSpan,
}

/// Summary of a single verification level.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EntityLevelSummary {
    /// Level name (e.g. "L0 SMT", "L1 Model Check").
    pub level: String,
    /// Whether this level passed.
    pub passed: bool,
    /// Human-readable summary.
    pub summary: String,
    /// Detailed violation information (populated only for failed levels).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Vec<VerificationDetail>>,
}

/// A single verification violation detail.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct VerificationDetail {
    /// Violation kind: "liveness_violation", "invariant_violation", "counterexample", "proptest_failure".
    pub kind: String,
    /// Property or invariant name that was violated.
    pub property: String,
    /// Human-readable description of the violation.
    pub description: String,
    /// Actor ID that triggered the violation (if applicable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor_id: Option<String>,
}

/// Errors raised while registering tenant specifications.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryError {
    /// cross-invariants TOML failed to parse.
    CrossInvariantParse { tenant: String, source: String },
    /// An IOA source failed to parse.
    IoaParse {
        tenant: String,
        entity_type: String,
        source: String,
    },
    /// An IOA declares safety invariants outside the supported contract.
    UnsupportedSafetyInvariants {
        tenant: String,
        entity_type: String,
        invariants: Vec<String>,
    },
    /// A hot swap attempted to change a runtime or model-proved safety contract
    /// without a state migration.
    RuntimeInvariantMigrationRequired { tenant: String, entity_type: String },
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CrossInvariantParse { tenant, source } => {
                write!(
                    f,
                    "failed to parse cross-invariants for tenant '{tenant}': {source}"
                )
            }
            Self::IoaParse {
                tenant,
                entity_type,
                source,
            } => {
                write!(
                    f,
                    "failed to parse IOA for tenant '{tenant}', entity '{entity_type}': {source}"
                )
            }
            Self::UnsupportedSafetyInvariants {
                tenant,
                entity_type,
                invariants,
            } => write!(
                f,
                "cannot activate IOA for tenant '{tenant}', entity '{entity_type}': unsupported safety invariants: {}",
                invariants.join(", ")
            ),
            Self::RuntimeInvariantMigrationRequired {
                tenant,
                entity_type,
            } => write!(
                f,
                "cannot hot-swap runtime or model-proved safety contract for tenant '{tenant}', entity '{entity_type}' without validating and migrating existing durable state"
            ),
        }
    }
}

impl std::error::Error for RegistryError {}

/// A compiled relation edge from CSDL navigation metadata.
#[derive(Debug, Clone)]
pub struct RelationEdge {
    /// Source entity type that owns the FK field.
    pub from_entity: String,
    /// Navigation property name on the source entity.
    pub navigation_property: String,
    /// Target entity type.
    pub to_entity: String,
    /// FK field on source entity (e.g. `OrderId`).
    pub source_field: String,
    /// Referenced key on target entity (usually `Id`).
    pub target_field: String,
    /// Whether the relationship allows null references.
    pub nullable: bool,
    /// Delete policy applied to this edge.
    pub delete_policy: DeletePolicy,
}

/// Tenant-scoped relation graph compiled from CSDL.
#[derive(Debug, Clone, Default)]
pub struct RelationGraph {
    /// Outgoing edges keyed by source entity type.
    pub outgoing: BTreeMap<String, Vec<RelationEdge>>,
    /// Incoming edges keyed by target entity type.
    pub incoming: BTreeMap<String, Vec<RelationEdge>>,
}

/// A registered tenant with its specs and entity configuration.
#[derive(Debug, Clone)]
pub struct TenantConfig {
    /// The CSDL document describing this tenant's entity model.
    pub csdl: Arc<CsdlDocument>,
    /// Raw CSDL XML for serving via `$metadata`.
    pub csdl_xml: Arc<String>,
    /// Maps entity set names to entity type names (from CSDL).
    pub entity_set_map: BTreeMap<String, String>,
    /// Per-entity-type specs.
    pub entities: BTreeMap<String, EntitySpec>,
    /// Reaction rules for cross-entity coordination.
    pub reactions: Vec<ReactionRule>,
    /// Tenant relation graph compiled from CSDL.
    pub relation_graph: RelationGraph,
    /// Optional parsed cross-entity invariant spec.
    pub cross_invariants: Option<CrossInvariantSpec>,
    /// Raw `cross-invariants.toml` source, if provided.
    pub cross_invariants_source: Option<String>,
    /// Indexed webhook routes: path -> (entity_type, Webhook).
    pub webhook_routes: BTreeMap<String, (String, Webhook)>,
    /// Per-entity verification status (design-time observation).
    pub verification: BTreeMap<String, VerificationStatus>,
}

/// A registered entity type's spec and transition table.
///
/// The table is wrapped in a [`SwapController`] to enable atomic hot-swap
/// without restarting actors. Use [`swap_controller()`] to access the
/// controller for hot-swap operations.
#[derive(Clone)]
pub struct EntitySpec {
    /// The parsed I/O Automaton specification.
    pub automaton: Automaton,
    /// Integration declarations from the IOA spec.
    pub integrations: Vec<Integration>,
    /// Hot-swappable transition table controller.
    pub(super) swap: Arc<SwapController>,
    /// Raw IOA TOML source (for invariant parsing, display, etc.).
    pub ioa_source: String,
}

impl std::fmt::Debug for EntitySpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EntitySpec")
            .field("automaton", &self.automaton)
            .field("version", &self.swap.version())
            .field("ioa_source_len", &self.ioa_source.len())
            .finish()
    }
}

impl EntitySpec {
    /// Get a snapshot of the current transition table.
    ///
    /// This reads through the [`SwapController`] — if a hot-swap happened,
    /// subsequent calls return the new table.
    pub fn table(&self) -> Arc<TransitionTable> {
        let lock = self.swap.current();
        let table = lock.read().expect("SwapController lock poisoned");
        // Clone the table out of the RwLock into an Arc for the caller.
        // This is cheap — TransitionTable is small (a few Vecs of strings).
        Arc::new(table.clone())
    }

    /// Get the [`SwapController`] for hot-swap operations.
    pub fn swap_controller(&self) -> &Arc<SwapController> {
        &self.swap
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_error_display_cross_invariant() {
        let err = RegistryError::CrossInvariantParse {
            tenant: "t1".into(),
            source: "bad toml".into(),
        };
        assert_eq!(
            err.to_string(),
            "failed to parse cross-invariants for tenant 't1': bad toml"
        );
    }

    #[test]
    fn registry_error_display_ioa_parse() {
        let err = RegistryError::IoaParse {
            tenant: "t1".into(),
            entity_type: "Order".into(),
            source: "missing states".into(),
        };
        assert_eq!(
            err.to_string(),
            "failed to parse IOA for tenant 't1', entity 'Order': missing states"
        );
    }

    #[test]
    fn registry_error_equality() {
        let e1 = RegistryError::CrossInvariantParse {
            tenant: "t1".into(),
            source: "x".into(),
        };
        let e2 = RegistryError::CrossInvariantParse {
            tenant: "t1".into(),
            source: "x".into(),
        };
        assert_eq!(e1, e2);
    }

    #[test]
    fn registry_error_ne_different_variant() {
        let e1 = RegistryError::CrossInvariantParse {
            tenant: "t1".into(),
            source: "x".into(),
        };
        let e2 = RegistryError::IoaParse {
            tenant: "t1".into(),
            entity_type: "E".into(),
            source: "x".into(),
        };
        assert_ne!(e1, e2);
    }

    #[test]
    fn verification_result_serde_roundtrip() {
        let result = EntityVerificationResult {
            all_passed: true,
            levels: vec![EntityLevelSummary {
                level: "L0 SMT".into(),
                passed: true,
                summary: "All checks passed".into(),
                details: None,
            }],
            warnings: vec!["runtime disclosure".into()],
            errors: Vec::new(),
            verified_at: "2025-01-01T00:00:00Z".into(),
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: EntityVerificationResult = serde_json::from_str(&json).unwrap();
        assert!(back.all_passed);
        assert_eq!(back.levels.len(), 1);
        assert!(back.levels[0].details.is_none());
        assert_eq!(back.warnings, vec!["runtime disclosure"]);
        assert!(back.errors.is_empty());
    }

    #[cfg(feature = "observe")]
    #[test]
    fn cascade_conversion_retains_capability_errors_and_disclosures() {
        let source = r#"
[automaton]
name = "Quota"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]

[[invariant]]
name = "UnsupportedQuota"
when = ["Active"]
assert = "used_bytes ** quota_limit"
"#;
        let mut cascade = temper_verify::VerificationCascade::from_ioa(source).run();
        cascade.warnings.push("runtime-only disclosure".to_string());

        let result =
            EntityVerificationResult::from_cascade(&cascade, "2026-07-20T00:00:00Z".to_string());

        assert!(!result.all_passed);
        assert_eq!(result.warnings, vec!["runtime-only disclosure"]);
        assert_eq!(result.errors.len(), 1);
        assert_eq!(result.errors[0].code, "TVE001");
        assert_eq!(result.errors[0].invariant, "UnsupportedQuota");
        assert_eq!(result.errors[0].assertion, "used_bytes ** quota_limit");
        assert!(result.levels.is_empty());
    }

    #[test]
    fn verification_detail_serde() {
        let detail = VerificationDetail {
            kind: "invariant_violation".into(),
            property: "TypeOK".into(),
            description: "Status not in valid set".into(),
            actor_id: Some("actor-1".into()),
        };
        let json = serde_json::to_string(&detail).unwrap();
        let back: VerificationDetail = serde_json::from_str(&json).unwrap();
        assert_eq!(back.kind, "invariant_violation");
        assert_eq!(back.actor_id, Some("actor-1".into()));
    }

    #[test]
    fn relation_graph_default_empty() {
        let g = RelationGraph::default();
        assert!(g.outgoing.is_empty());
        assert!(g.incoming.is_empty());
    }
}
