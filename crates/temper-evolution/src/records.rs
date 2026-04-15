//! Evolution record types: O, P, A, D, I, FR.
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
    /// FeatureRequest: platform gap detected, needs developer review.
    FeatureRequest,
}

impl RecordType {
    /// Return the single-character prefix for this record type (e.g., "O", "P", "A").
    pub fn prefix(&self) -> &'static str {
        match self {
            RecordType::Observation => "O",
            RecordType::Problem => "P",
            RecordType::Analysis => "A",
            RecordType::Decision => "D",
            RecordType::Insight => "I",
            RecordType::FeatureRequest => "FR",
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
    /// Performance degradation (latency, throughput).
    Performance,
    /// Error rate anomaly.
    ErrorRate,
    /// State machine invariant or liveness violation.
    StateMachine,
    /// Security-related event.
    Security,
    /// Trajectory pattern anomaly.
    Trajectory,
    /// Authorization denial event (Cedar policy denied a WASM call).
    AuthzDenied,
    /// Resource usage anomaly (CPU, memory, connections).
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

/// Severity level for an impact assessment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Low severity — cosmetic or minor usability issue.
    Low,
    /// Medium severity — degraded functionality for some users.
    Medium,
    /// High severity — major functionality loss.
    High,
    /// Critical severity — system outage or data integrity risk.
    Critical,
}

/// Trend direction for an observation or metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Trend {
    /// Getting worse over time.
    Growing,
    /// Holding steady.
    Stable,
    /// Improving over time.
    Declining,
}

/// Complexity estimate for a solution option.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Complexity {
    /// Trivial change (minutes).
    Trivial,
    /// Low complexity (hours).
    Low,
    /// Medium complexity (days).
    Medium,
    /// High complexity (weeks).
    High,
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Severity::Low => write!(f, "low"),
            Severity::Medium => write!(f, "medium"),
            Severity::High => write!(f, "high"),
            Severity::Critical => write!(f, "critical"),
        }
    }
}

impl std::fmt::Display for Trend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Trend::Growing => write!(f, "growing"),
            Trend::Stable => write!(f, "stable"),
            Trend::Declining => write!(f, "declining"),
        }
    }
}

impl std::fmt::Display for Complexity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Complexity::Trivial => write!(f, "trivial"),
            Complexity::Low => write!(f, "low"),
            Complexity::Medium => write!(f, "medium"),
            Complexity::High => write!(f, "high"),
        }
    }
}

impl std::fmt::Display for SolutionRisk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SolutionRisk::None => write!(f, "none"),
            SolutionRisk::Low => write!(f, "low"),
            SolutionRisk::Medium => write!(f, "medium"),
            SolutionRisk::High => write!(f, "high"),
        }
    }
}

/// Impact assessment for a problem.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImpactAssessment {
    /// Estimated number of affected users per time period.
    pub affected_users: Option<u64>,
    /// Severity level.
    pub severity: Severity,
    /// Trend direction.
    pub trend: Trend,
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

/// Risk level for a solution option.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SolutionRisk {
    /// No risk — purely additive, cannot affect correctness.
    None,
    /// Low risk — minor behavioural change, easily reversible.
    Low,
    /// Medium risk — requires shadow testing.
    Medium,
    /// High risk — significant correctness or availability implications.
    High,
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
    /// Risk level.
    pub risk: SolutionRisk,
    /// Estimated complexity.
    pub complexity: Complexity,
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
    /// The proposed change is approved.
    Approved,
    /// The proposed change is rejected.
    Rejected,
    /// The decision is deferred for later.
    Deferred,
}

/// Summary of verification cascade results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationSummary {
    /// Whether the Stateright model check passed.
    pub stateright_pass: bool,
    /// Number of states explored by Stateright.
    pub stateright_states_explored: u64,
    /// Whether the deterministic simulation passed.
    pub simulation_pass: bool,
    /// Whether property-based tests passed.
    pub proptest_pass: bool,
    /// Number of proptest cases executed.
    pub proptest_cases: u64,
}

/// Implementation plan for an approved change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImplementationPlan {
    /// The codegen command to run (e.g., "temper codegen --from-spec v47").
    pub codegen_command: String,
    /// Whether a database migration is required.
    pub migration_required: bool,
    /// Deployment strategy (e.g., "Rolling restart", "Blue-green").
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
    /// Platform-level capability gap (feeds into FeatureRequest creation).
    PlatformGap,
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
    pub trend: Trend,
    /// Growth rate (week-over-week).
    pub growth_rate: Option<f64>,
}

// ============================================================
// Feature Request Record (FR-Record) — Platform Gap Intelligence
// ============================================================

/// Category of platform capability gap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlatformGapCategory {
    /// Unknown MCP method called.
    MissingMethod,
    /// Method exists but is governance-blocked for agents.
    GovernanceBlocked,
    /// Integration type not supported.
    UnsupportedIntegration,
    /// General platform limitation.
    MissingCapability,
}

/// Disposition of a feature request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FeatureRequestDisposition {
    /// Just detected, not yet reviewed.
    Open,
    /// Developer has seen it.
    Acknowledged,
    /// Developer intends to implement.
    Planned,
    /// Developer decided not to.
    WontFix,
    /// Implemented.
    Resolved,
}

/// A feature request record derived from platform-level gaps.
///
/// Created when the platform itself can't do something (unknown MCP method,
/// unsupported integration type, missing capability). Extends the O-P-A-D-I
/// chain to O-P-A-D-I-FR.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeatureRequestRecord {
    pub header: RecordHeader,
    /// What kind of platform gap.
    pub category: PlatformGapCategory,
    /// Human-readable description of the gap.
    pub description: String,
    /// How many times this gap was hit.
    pub frequency: u64,
    /// Links to trajectory timestamps.
    pub trajectory_refs: Vec<String>,
    /// Current disposition.
    pub disposition: FeatureRequestDisposition,
    /// Developer notes (set on review).
    pub developer_notes: Option<String>,
}

// ============================================================
// Constructors
// ============================================================

impl RecordHeader {
    /// Create a new record header with auto-generated ID.
    ///
    /// Uses 12 hex characters from a UUID v7 suffix (~48 bits of entropy)
    /// to avoid collisions in high-throughput workloads.
    pub fn new(record_type: RecordType, created_by: impl Into<String>) -> Self {
        let now = Utc::now();
        let year = now.format("%Y");
        let full_uuid = uuid::Uuid::now_v7().to_string();
        // Take the last 12 hex chars (skipping hyphens) for ~48 bits of entropy.
        let hex_chars: String = full_uuid.chars().filter(|c| *c != '-').collect();
        let suffix = &hex_chars[hex_chars.len() - 12..];
        let id = format!("{}-{}-{}", record_type.prefix(), year, suffix);

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

        let p_header = RecordHeader::new(RecordType::Problem, "agent").derived_from(&o_id);

        assert_eq!(p_header.derived_from, Some(o_id.clone()));
        assert!(p_header.id.starts_with("P-"));

        let a_header = RecordHeader::new(RecordType::Analysis, "agent").derived_from(&p_header.id);
        assert!(a_header.id.starts_with("A-"));
        assert_eq!(a_header.derived_from, Some(p_header.id));
    }

    #[test]
    fn test_observation_record() {
        let record = ObservationRecord {
            header: RecordHeader::new(RecordType::Observation, "sentinel:latency"),
            source: "sentinel:latency".to_string(),
            classification: ObservationClass::Performance,
            evidence_query: "SELECT p99(duration_ns) FROM spans WHERE operation = 'handle'"
                .to_string(),
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
                trend: Trend::Growing,
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
        assert!(
            decision
                .verification_results
                .as_ref()
                .unwrap()
                .stateright_pass
        );
    }

    #[test]
    fn test_record_type_prefixes() {
        assert_eq!(RecordType::Observation.prefix(), "O");
        assert_eq!(RecordType::Problem.prefix(), "P");
        assert_eq!(RecordType::Analysis.prefix(), "A");
        assert_eq!(RecordType::Decision.prefix(), "D");
        assert_eq!(RecordType::Insight.prefix(), "I");
        assert_eq!(RecordType::FeatureRequest.prefix(), "FR");
    }

    #[test]
    fn test_feature_request_record() {
        let record = FeatureRequestRecord {
            header: RecordHeader::new(RecordType::FeatureRequest, "insight-generator"),
            category: PlatformGapCategory::MissingMethod,
            description: "Agents tried 'send_email' 47 times".to_string(),
            frequency: 47,
            trajectory_refs: vec!["2024-01-01T00:00:00Z".to_string()],
            disposition: FeatureRequestDisposition::Open,
            developer_notes: None,
        };

        assert!(record.header.id.starts_with("FR-"));
        assert_eq!(record.category, PlatformGapCategory::MissingMethod);
        assert_eq!(record.frequency, 47);
        assert_eq!(record.disposition, FeatureRequestDisposition::Open);
    }

    #[test]
    fn test_feature_request_serialization() {
        let record = FeatureRequestRecord {
            header: RecordHeader::new(RecordType::FeatureRequest, "insight-generator"),
            category: PlatformGapCategory::GovernanceBlocked,
            description: "Method 'set_policy' blocked for agents".to_string(),
            frequency: 12,
            trajectory_refs: vec![],
            disposition: FeatureRequestDisposition::Acknowledged,
            developer_notes: Some("Will add a scoped version".to_string()),
        };

        let json = serde_json::to_string(&record).unwrap();
        let deserialized: FeatureRequestRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.header.id, record.header.id);
        assert_eq!(
            deserialized.category,
            PlatformGapCategory::GovernanceBlocked
        );
        assert_eq!(
            deserialized.disposition,
            FeatureRequestDisposition::Acknowledged
        );
    }

    #[test]
    fn test_insight_category_platform_gap() {
        let record = InsightRecord {
            header: RecordHeader::new(RecordType::Insight, "insight-generator"),
            category: InsightCategory::PlatformGap,
            signal: InsightSignal {
                intent: "unknown method 'send_email'".to_string(),
                volume: 47,
                success_rate: 0.0,
                trend: Trend::Growing,
                growth_rate: Some(0.25),
            },
            recommendation: "Consider adding email integration capability".to_string(),
            priority_score: 0.9,
        };

        assert_eq!(record.category, InsightCategory::PlatformGap);
    }
}
