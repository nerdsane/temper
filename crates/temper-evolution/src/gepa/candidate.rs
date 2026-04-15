//! Candidate tracking for GEPA evolution runs.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Status of a candidate in the evolution pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateStatus {
    /// Newly proposed, not yet evaluated.
    Proposed,
    /// Currently being evaluated (replay + scoring).
    Evaluating,
    /// Evaluation complete, awaiting verification.
    Scored,
    /// Passed L0-L3 verification cascade.
    Verified,
    /// Failed verification cascade.
    VerificationFailed,
    /// Approved for deployment.
    Approved,
    /// Deployed to production.
    Deployed,
    /// Rejected by human or policy.
    Rejected,
}

/// A candidate spec mutation in the GEPA evolutionary process.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    /// Unique candidate identifier.
    pub id: String,

    /// The full IOA spec source for this candidate.
    pub spec_source: String,

    /// Skill (OS app) this candidate targets.
    pub skill_name: String,

    /// Entity type within the skill this mutation affects.
    pub entity_type: String,

    /// Multi-objective scores (objective_name → score).
    pub scores: BTreeMap<String, f64>,

    /// Generation number (0 = original spec, 1+ = mutations).
    pub generation: u32,

    /// ID of the parent candidate this was mutated from.
    pub parent_id: Option<String>,

    /// Current status.
    pub status: CandidateStatus,

    /// Number of mutation attempts for this candidate.
    pub mutation_attempts: u32,

    /// When this candidate was created.
    pub created_at: DateTime<Utc>,

    /// Summary of the mutation (what changed and why).
    pub mutation_summary: Option<String>,

    /// Verification errors from the cascade (if any).
    pub verification_errors: Vec<String>,
}

impl Candidate {
    /// Create a new candidate from a proposed spec mutation.
    pub fn new(
        id: String,
        spec_source: String,
        skill_name: String,
        entity_type: String,
        generation: u32,
        created_at: DateTime<Utc>,
    ) -> Self {
        Self {
            id,
            spec_source,
            skill_name,
            entity_type,
            scores: BTreeMap::new(),
            generation,
            parent_id: None,
            status: CandidateStatus::Proposed,
            mutation_attempts: 0,
            created_at,
            mutation_summary: None,
            verification_errors: Vec::new(),
        }
    }

    /// Set the parent candidate ID.
    pub fn with_parent(mut self, parent_id: String) -> Self {
        self.parent_id = Some(parent_id);
        self
    }

    /// Set the mutation summary.
    pub fn with_mutation_summary(mut self, summary: String) -> Self {
        self.mutation_summary = Some(summary);
        self
    }

    /// Record a score for an objective.
    pub fn set_score(&mut self, objective: String, score: f64) {
        self.scores.insert(objective, score);
    }

    /// Record verification failure.
    pub fn record_verification_failure(&mut self, errors: Vec<String>) {
        self.status = CandidateStatus::VerificationFailed;
        self.verification_errors = errors;
        self.mutation_attempts += 1;
    }

    /// Check if the candidate has exceeded the mutation attempt budget.
    pub fn exceeded_budget(&self, max_attempts: u32) -> bool {
        self.mutation_attempts >= max_attempts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn test_candidate_creation() {
        let now = Utc::now();
        let candidate = Candidate::new(
            "c1".into(),
            "spec source".into(),
            "project-management".into(),
            "Issue".into(),
            1,
            now,
        )
        .with_parent("c0".into())
        .with_mutation_summary("Added Reassign action".into());

        assert_eq!(candidate.id, "c1");
        assert_eq!(candidate.generation, 1);
        assert_eq!(candidate.parent_id, Some("c0".into()));
        assert_eq!(candidate.status, CandidateStatus::Proposed);
        assert_eq!(candidate.mutation_attempts, 0);
    }

    #[test]
    fn test_candidate_scoring() {
        let now = Utc::now();
        let mut candidate = Candidate::new(
            "c1".into(),
            "spec".into(),
            "pm".into(),
            "Issue".into(),
            1,
            now,
        );

        candidate.set_score("success_rate".into(), 0.85);
        candidate.set_score("coverage".into(), 0.92);

        assert_eq!(candidate.scores.len(), 2);
        assert_eq!(candidate.scores["success_rate"], 0.85);
    }

    #[test]
    fn test_verification_failure_tracking() {
        let now = Utc::now();
        let mut candidate = Candidate::new(
            "c1".into(),
            "spec".into(),
            "pm".into(),
            "Issue".into(),
            1,
            now,
        );

        candidate.record_verification_failure(vec!["invariant violated".into()]);
        assert_eq!(candidate.status, CandidateStatus::VerificationFailed);
        assert_eq!(candidate.mutation_attempts, 1);
        assert!(!candidate.exceeded_budget(3));

        candidate.record_verification_failure(vec!["guard unsatisfiable".into()]);
        candidate.record_verification_failure(vec!["dead transition".into()]);
        assert!(candidate.exceeded_budget(3));
    }

    #[test]
    fn test_candidate_serialization() {
        let now = Utc::now();
        let mut candidate = Candidate::new(
            "c1".into(),
            "spec".into(),
            "pm".into(),
            "Issue".into(),
            1,
            now,
        );
        candidate.set_score("success_rate".into(), 0.9);

        let json = serde_json::to_string(&candidate).unwrap();
        let parsed: Candidate = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, "c1");
        assert_eq!(parsed.scores["success_rate"], 0.9);
    }
}
