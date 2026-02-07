//! Evolution record types: O, P, A, D, I.
//!
//! Each record is immutable, timestamped, and links to its predecessors.
//! Records are the system's institutional memory — every change to the
//! system has observable evidence, a formal problem statement, proven
//! correctness, and a human decision attached.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Unique identifier for an evolution record.
pub type RecordId = String;

/// The type of evolution record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RecordType {
    /// Observation: detected anomaly, SLO violation, or pattern from production.
    Observation,
    /// Problem: Lamport-style formal problem statement derived from observation.
    Problem,
    /// Analysis: root cause analysis with proposed solutions and spec diffs.
    Analysis,
    /// Decision: human approval/rejection of a proposed change.
    Decision,
    /// Insight: product intelligence derived from trajectory analysis.
    Insight,
}

impl RecordType {
    pub fn prefix(&self) -> &'static str {
        match self {
            RecordType::Observation => "O",
            RecordType::Problem => "P",
            RecordType::Analysis => "A",
            RecordType::Decision => "D",
            RecordType::Insight => "I",
        }
    }
}

/// Common header shared by all record types.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordHeader {
    /// Unique record ID (e.g., "O-2024-0042").
    pub id: RecordId,
    /// Record type.
    pub record_type: RecordType,
    /// When this record was created.
    pub timestamp: DateTime<Utc>,
    /// Who/what created this record.
    pub created_by: String,
    /// ID of the record this derives from (None for root observations).
    pub derived_from: Option<RecordId>,
    /// Current status of this record.
    pub status: RecordStatus,
}

/// Status of an evolution record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecordStatus {
    /// Record is open and active.
    Open,
    /// Record has been addressed/resolved.
    Resolved,
    /// Record was superseded by another.
    Superseded,
    /// Record was rejected (for decisions).
    Rejected,
}

// ============================================================
// Observation Record (O-Record)
// ============================================================

/// An observation from production telemetry.
/// Created by SentinelActors watching Logfire/Datadog.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservationRecord {
    pub header: RecordHeader,
    /// The source that detected this (e.g., "sentinel:latency", "sentinel:error_rate").
    pub source: String,
    /// Classification of the observation.
    pub classification: ObservationClass,
    /// Evidence: the SQL query that detected this (portable, provider-agnostic).
    pub evidence_query: String,
    /// The metric/field that triggered the observation.
    pub threshold_field: Option<String>,
    /// Expected threshold value.
    pub threshold_value: Option<f64>,
    /// Actual observed value.
    pub observed_value: Option<f64>,
    /// Additional context as key-value pairs.
    pub context: serde_json::Value,
}

/// Classification of an observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ObservationClass {
    Performance,
    ErrorRate,
    StateMachine,
    Security,
    Trajectory,
    ResourceUsage,
}

// ============================================================
// Problem Record (P-Record) — Lamport-style
// ============================================================

/// A formal problem statement derived from an observation.
/// Follows Lamport's method: state the problem precisely before solving it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProblemRecord {
    pub header: RecordHeader,
    /// The formal problem statement (may include mathematical notation).
    pub problem_statement: String,
    /// Invariants that must continue to hold in any solution.
    pub invariants: Vec<String>,
    /// Constraints on the solution space.
    pub constraints: Vec<String>,
    /// Impact assessment.
    pub impact: ImpactAssessment,
}

/// Impact assessment for a problem.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImpactAssessment {
    /// Estimated number of affected users per time period.
    pub affected_users: Option<u64>,
    /// Severity: low, medium, high, critical.
    pub severity: String,
    /// Trend: growing, stable, declining.
    pub trend: String,
}

// ============================================================
// Analysis Record (A-Record)
// ============================================================

/// Root cause analysis with proposed solutions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalysisRecord {
    pub header: RecordHeader,
    /// Root cause description.
    pub root_cause: String,
    /// Proposed solution options.
    pub options: Vec<SolutionOption>,
    /// Recommended option index.
    pub recommendation: Option<usize>,
}

/// A proposed solution option.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SolutionOption {
    /// Short description of the option.
    pub description: String,
    /// Spec changes required (CSDL diffs, TLA+ changes, Cedar policy changes).
    pub spec_diff: String,
    /// Impact on TLA+ invariants ("NONE" if no invariant changes).
    pub tla_impact: String,
    /// Risk level: low, medium, high.
    pub risk: String,
    /// Estimated complexity.
    pub complexity: String,
}

// ============================================================
// Decision Record (D-Record)
// ============================================================

/// A human decision on a proposed change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionRecord {
    pub header: RecordHeader,
    /// The decision: approve, reject, defer.
    pub decision: Decision,
    /// Who approved/rejected.
    pub decided_by: String,
    /// Human rationale for the decision.
    pub rationale: String,
    /// Verification cascade results (if approved).
    pub verification_results: Option<VerificationSummary>,
    /// Implementation details (if approved).
    pub implementation: Option<ImplementationPlan>,
}

/// The decision outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Decision {
    Approved,
    Rejected,
    Deferred,
}

/// Summary of verification cascade results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationSummary {
    pub stateright_pass: bool,
    pub stateright_states_explored: u64,
    pub simulation_pass: bool,
    pub proptest_pass: bool,
    pub proptest_cases: u64,
}

/// Implementation plan for an approved change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImplementationPlan {
    pub codegen_command: String,
    pub migration_required: bool,
    pub deployment_strategy: String,
}

// ============================================================
// Insight Record (I-Record) — Product Intelligence
// ============================================================

/// Product intelligence derived from trajectory analysis.
/// Tells the human creator what to build next.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InsightRecord {
    pub header: RecordHeader,
    /// Category of insight.
    pub category: InsightCategory,
    /// The user intent or pattern detected.
    pub signal: InsightSignal,
    /// Recommendation for what to build.
    pub recommendation: String,
    /// Priority score (0.0 to 1.0, computed from volume × impact × trend).
    pub priority_score: f64,
}

/// Category of product insight.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InsightCategory {
    /// Users want something that doesn't exist.
    UnmetIntent,
    /// Something works but takes too many steps.
    Friction,
    /// Agents are hacking around a gap.
    Workaround,
}

/// Signal data for an insight.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InsightSignal {
    /// The detected intent or pattern.
    pub intent: String,
    /// How many trajectories exhibited this.
    pub volume: u64,
    /// Success/failure rate for this intent.
    pub success_rate: f64,
    /// Trend direction.
    pub trend: String,
    /// Growth rate (week-over-week).
    pub growth_rate: Option<f64>,
}

// ============================================================
// Constructors
// ============================================================

impl RecordHeader {
    /// Create a new record header with auto-generated ID.
    pub fn new(record_type: RecordType, created_by: impl Into<String>) -> Self {
        let now = Utc::now();
        let year = now.format("%Y");
        let seq = &uuid::Uuid::now_v7().to_string()[..4];
        let id = format!("{}-{}-{}", record_type.prefix(), year, seq);

        Self {
            id,
            record_type,
            timestamp: now,
            created_by: created_by.into(),
            derived_from: None,
            status: RecordStatus::Open,
        }
    }

    /// Set the derived_from link.
    pub fn derived_from(mut self, parent_id: impl Into<String>) -> Self {
        self.derived_from = Some(parent_id.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_record_header_creation() {
        let header = RecordHeader::new(RecordType::Observation, "sentinel:latency");
        assert!(header.id.starts_with("O-"));
        assert_eq!(header.record_type, RecordType::Observation);
        assert_eq!(header.created_by, "sentinel:latency");
        assert_eq!(header.status, RecordStatus::Open);
        assert!(header.derived_from.is_none());
    }

    #[test]
    fn test_record_chain_linking() {
        let o_header = RecordHeader::new(RecordType::Observation, "sentinel");
        let o_id = o_header.id.clone();

        let p_header = RecordHeader::new(RecordType::Problem, "agent")
            .derived_from(&o_id);

        assert_eq!(p_header.derived_from, Some(o_id.clone()));
        assert!(p_header.id.starts_with("P-"));

        let a_header = RecordHeader::new(RecordType::Analysis, "agent")
            .derived_from(&p_header.id);
        assert!(a_header.id.starts_with("A-"));
        assert_eq!(a_header.derived_from, Some(p_header.id));
    }

    #[test]
    fn test_observation_record() {
        let record = ObservationRecord {
            header: RecordHeader::new(RecordType::Observation, "sentinel:latency"),
            source: "sentinel:latency".to_string(),
            classification: ObservationClass::Performance,
            evidence_query: "SELECT p99(duration_ns) FROM spans WHERE operation = 'handle'".to_string(),
            threshold_field: Some("p99".to_string()),
            threshold_value: Some(100_000_000.0),
            observed_value: Some(450_000_000.0),
            context: serde_json::json!({"concurrent_actors": 12847}),
        };

        assert_eq!(record.classification, ObservationClass::Performance);
        assert!(record.observed_value.unwrap() > record.threshold_value.unwrap());
    }

    #[test]
    fn test_insight_record() {
        let record = InsightRecord {
            header: RecordHeader::new(RecordType::Insight, "trajectory_analyzer"),
            category: InsightCategory::UnmetIntent,
            signal: InsightSignal {
                intent: "split order into multiple shipments".to_string(),
                volume: 234,
                success_rate: 0.18,
                trend: "growing".to_string(),
                growth_rate: Some(0.12),
            },
            recommendation: "Add SplitOrder action to Order entity".to_string(),
            priority_score: 0.87,
        };

        assert_eq!(record.category, InsightCategory::UnmetIntent);
        assert_eq!(record.signal.volume, 234);
        assert!(record.priority_score > 0.5);
    }

    #[test]
    fn test_full_record_chain_serialization() {
        let obs = ObservationRecord {
            header: RecordHeader::new(RecordType::Observation, "sentinel"),
            source: "sentinel:latency".into(),
            classification: ObservationClass::Performance,
            evidence_query: "SELECT count(*) FROM spans".into(),
            threshold_field: None,
            threshold_value: None,
            observed_value: None,
            context: serde_json::json!({}),
        };

        let json = serde_json::to_string(&obs).unwrap();
        let deserialized: ObservationRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.header.id, obs.header.id);
        assert_eq!(deserialized.source, obs.source);
    }

    #[test]
    fn test_decision_record() {
        let decision = DecisionRecord {
            header: RecordHeader::new(RecordType::Decision, "human:alice")
                .derived_from("A-2024-abc1"),
            decision: Decision::Approved,
            decided_by: "alice@company.com".to_string(),
            rationale: "Low risk, addresses root cause directly".to_string(),
            verification_results: Some(VerificationSummary {
                stateright_pass: true,
                stateright_states_explored: 42847,
                simulation_pass: true,
                proptest_pass: true,
                proptest_cases: 100_000,
            }),
            implementation: Some(ImplementationPlan {
                codegen_command: "temper codegen --from-spec v47".to_string(),
                migration_required: false,
                deployment_strategy: "Rolling restart".to_string(),
            }),
        };

        assert_eq!(decision.decision, Decision::Approved);
        assert!(decision.verification_results.as_ref().unwrap().stateright_pass);
    }

    #[test]
    fn test_record_type_prefixes() {
        assert_eq!(RecordType::Observation.prefix(), "O");
        assert_eq!(RecordType::Problem.prefix(), "P");
        assert_eq!(RecordType::Analysis.prefix(), "A");
        assert_eq!(RecordType::Decision.prefix(), "D");
        assert_eq!(RecordType::Insight.prefix(), "I");
    }
}
