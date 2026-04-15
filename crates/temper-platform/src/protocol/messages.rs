//! Platform event protocol.
//!
//! All internal platform events are typed via [`PlatformEvent`]. These replace
//! the old WebSocket messages — they're used for internal broadcast between
//! subsystems (deploy pipeline, evolution engine, optimization loop).

use serde::{Deserialize, Serialize};

/// Platform event broadcast envelope.
///
/// Emitted by the deploy pipeline, evolution engine, and optimization loop.
/// Subscribers (e.g., background tasks, logging) receive these via the
/// broadcast channel in [`PlatformState`](crate::state::PlatformState).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PlatformEvent {
    /// A tenant was registered in the SpecRegistry.
    TenantRegistered {
        /// Tenant name.
        tenant: String,
        /// Number of entity types in the tenant.
        entity_count: usize,
    },

    /// A tenant was removed from the SpecRegistry.
    TenantRemoved {
        /// Tenant name.
        tenant: String,
    },

    /// Verification cascade status update.
    VerifyStatus {
        /// Tenant being verified.
        tenant: String,
        /// Which cascade level or entity.
        level: String,
        /// Current status.
        status: VerifyStepStatus,
        /// Human-readable summary.
        summary: String,
    },

    /// Deployment completed (success or failure).
    DeployStatus {
        /// Tenant that was deployed.
        tenant: String,
        /// Whether deployment succeeded.
        success: bool,
        /// Human-readable summary.
        summary: String,
    },

    /// Catalog entry published.
    CatalogPublished {
        /// Application ID in the catalog.
        app_id: String,
        /// Application name.
        name: String,
    },

    /// Evolution engine event (O-Record, I-Record, approval, etc.).
    EvolutionEvent {
        /// What kind of evolution event.
        event_type: String,
        /// Summary of the event.
        summary: String,
        /// Associated record ID.
        record_id: String,
    },

    /// An optimization recommendation was automatically applied.
    OptimizationApplied {
        /// Which optimizer produced this.
        optimizer: String,
        /// What action was taken.
        action: String,
        /// Estimated improvement (0.0–1.0).
        improvement: f64,
    },

    /// An optimization recommendation was proposed (needs approval).
    OptimizationProposed {
        /// Which optimizer produced this.
        optimizer: String,
        /// Description of the recommendation.
        description: String,
        /// Risk level.
        risk: String,
        /// Record ID for tracking.
        record_id: String,
    },

    /// Error event.
    Error {
        /// Error description.
        message: String,
    },
}

/// Status of a verification step.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VerifyStepStatus {
    /// Step is queued but not started.
    Pending,
    /// Step is currently running.
    Running,
    /// Step passed.
    Passed,
    /// Step failed.
    Failed,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tenant_registered_roundtrip() {
        let msg = PlatformEvent::TenantRegistered {
            tenant: "alpha".into(),
            entity_count: 3,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"tenant_registered\""));
        let parsed: PlatformEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            PlatformEvent::TenantRegistered {
                tenant,
                entity_count,
            } => {
                assert_eq!(tenant, "alpha");
                assert_eq!(entity_count, 3);
            }
            _ => panic!("expected TenantRegistered"),
        }
    }

    #[test]
    fn test_verify_status_roundtrip() {
        let msg = PlatformEvent::VerifyStatus {
            tenant: "test".into(),
            level: "L1 Model Check".into(),
            status: VerifyStepStatus::Running,
            summary: "Exploring state space...".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: PlatformEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            PlatformEvent::VerifyStatus { level, status, .. } => {
                assert_eq!(level, "L1 Model Check");
                assert_eq!(status, VerifyStepStatus::Running);
            }
            _ => panic!("expected VerifyStatus"),
        }
    }

    #[test]
    fn test_deploy_status_roundtrip() {
        let msg = PlatformEvent::DeployStatus {
            tenant: "test".into(),
            success: true,
            summary: "Deployed 2 entities".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: PlatformEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            PlatformEvent::DeployStatus { success, .. } => assert!(success),
            _ => panic!("expected DeployStatus"),
        }
    }

    #[test]
    fn test_evolution_event_roundtrip() {
        let msg = PlatformEvent::EvolutionEvent {
            event_type: "unmet_intent".into(),
            summary: "User wants split order".into(),
            record_id: "I-abc".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: PlatformEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            PlatformEvent::EvolutionEvent { event_type, .. } => {
                assert_eq!(event_type, "unmet_intent");
            }
            _ => panic!("expected EvolutionEvent"),
        }
    }

    #[test]
    fn test_error_roundtrip() {
        let msg = PlatformEvent::Error {
            message: "Something went wrong".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: PlatformEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            PlatformEvent::Error { message } => assert_eq!(message, "Something went wrong"),
            _ => panic!("expected Error"),
        }
    }

    #[test]
    fn test_optimization_applied_roundtrip() {
        let msg = PlatformEvent::OptimizationApplied {
            optimizer: "CacheOptimizer".into(),
            action: "UpdateCacheTtl".into(),
            improvement: 0.25,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: PlatformEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            PlatformEvent::OptimizationApplied {
                optimizer,
                improvement,
                ..
            } => {
                assert_eq!(optimizer, "CacheOptimizer");
                assert!((improvement - 0.25).abs() < f64::EPSILON);
            }
            _ => panic!("expected OptimizationApplied"),
        }
    }

    #[test]
    fn test_optimization_proposed_roundtrip() {
        let msg = PlatformEvent::OptimizationProposed {
            optimizer: "QueryOptimizer".into(),
            description: "Batch N+1 queries".into(),
            risk: "Medium".into(),
            record_id: "O-xyz".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: PlatformEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            PlatformEvent::OptimizationProposed { risk, .. } => {
                assert_eq!(risk, "Medium");
            }
            _ => panic!("expected OptimizationProposed"),
        }
    }
}
