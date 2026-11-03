//! Annotation models for trajectory evaluation
//!
//! DST adaptations:
//! - `OTSAnnotation::new()` uses `sim_uuid()` for ID generation
//! - All constructors accept `DateTime<Utc>` instead of calling `Utc::now()`

use crate::models::EvaluatorType;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use temper_runtime::scheduler::sim_uuid;

/// Evaluator information
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OTSEvaluator {
    /// Evaluator identifier
    pub id: String,

    /// Evaluator type
    #[serde(rename = "type")]
    pub evaluator_type: EvaluatorType,

    /// Evaluator version
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

impl OTSEvaluator {
    /// Create a new evaluator
    pub fn new(id: impl Into<String>, evaluator_type: EvaluatorType) -> Self {
        Self {
            id: id.into(),
            evaluator_type,
            version: None,
        }
    }

    /// Set the version
    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = Some(version.into());
        self
    }
}

/// Linked annotation for trajectory, turn, or decision
///
/// Annotations are separate from trajectories for:
/// - Multiple evaluators per trajectory
/// - Retroactive annotations
/// - Different retention policies
///
/// DST adaptation: uses `sim_uuid()` for annotation ID generation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OTSAnnotation {
    /// Unique annotation identifier
    pub annotation_id: String,

    /// Trajectory this annotates
    pub trajectory_id: String,

    /// Turn ID (None = trajectory-level annotation)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<i32>,

    /// Decision ID (None = turn-level annotation)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision_id: Option<String>,

    /// Evaluator information
    pub evaluator: OTSEvaluator,

    /// Evaluation score (0.0 to 1.0)
    pub score: f64,

    /// Label or category
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,

    /// Feedback text
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback: Option<String>,

    /// When annotation was created
    pub timestamp: DateTime<Utc>,
}

impl OTSAnnotation {
    /// Create a new annotation at trajectory level.
    ///
    /// Uses `sim_uuid()` for deterministic ID generation.
    /// Accepts an explicit `timestamp` instead of calling `Utc::now()`.
    pub fn new(
        trajectory_id: impl Into<String>,
        evaluator: OTSEvaluator,
        score: f64,
        timestamp: DateTime<Utc>,
    ) -> Self {
        assert!(
            (0.0..=1.0).contains(&score),
            "Score must be between 0.0 and 1.0, got {}",
            score
        );
        Self {
            annotation_id: sim_uuid().to_string(),
            trajectory_id: trajectory_id.into(),
            turn_id: None,
            decision_id: None,
            evaluator,
            score,
            label: None,
            feedback: None,
            timestamp,
        }
    }

    /// Create a turn-level annotation
    pub fn for_turn(
        trajectory_id: impl Into<String>,
        turn_id: i32,
        evaluator: OTSEvaluator,
        score: f64,
        timestamp: DateTime<Utc>,
    ) -> Self {
        let mut annotation = Self::new(trajectory_id, evaluator, score, timestamp);
        annotation.turn_id = Some(turn_id);
        annotation
    }

    /// Create a decision-level annotation
    pub fn for_decision(
        trajectory_id: impl Into<String>,
        turn_id: i32,
        decision_id: impl Into<String>,
        evaluator: OTSEvaluator,
        score: f64,
        timestamp: DateTime<Utc>,
    ) -> Self {
        let mut annotation = Self::for_turn(trajectory_id, turn_id, evaluator, score, timestamp);
        annotation.decision_id = Some(decision_id.into());
        annotation
    }

    /// Set the annotation ID
    pub fn with_annotation_id(mut self, annotation_id: impl Into<String>) -> Self {
        self.annotation_id = annotation_id.into();
        self
    }

    /// Set the label
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Set the feedback
    pub fn with_feedback(mut self, feedback: impl Into<String>) -> Self {
        self.feedback = Some(feedback.into());
        self
    }

    /// Set the timestamp
    pub fn with_timestamp(mut self, timestamp: DateTime<Utc>) -> Self {
        self.timestamp = timestamp;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use temper_runtime::scheduler::sim_now;

    #[test]
    #[should_panic(expected = "Score must be between 0.0 and 1.0")]
    fn test_annotation_invalid_score() {
        let now = sim_now();
        let evaluator = OTSEvaluator::new("test", EvaluatorType::Human);
        OTSAnnotation::new("traj", evaluator, 1.5, now);
    }

    #[test]
    fn test_annotation_levels() {
        let now = sim_now();
        let eval1 = OTSEvaluator::new("e1", EvaluatorType::Human);
        let eval2 = OTSEvaluator::new("e2", EvaluatorType::Model);
        let eval3 = OTSEvaluator::new("e3", EvaluatorType::Heuristic);

        // Trajectory-level: no turn_id, no decision_id
        let traj_ann = OTSAnnotation::new("t1", eval1, 0.8, now);
        assert!(traj_ann.turn_id.is_none());
        assert!(traj_ann.decision_id.is_none());

        // Turn-level: has turn_id, no decision_id
        let turn_ann = OTSAnnotation::for_turn("t1", 1, eval2, 0.7, now);
        assert!(turn_ann.turn_id.is_some());
        assert!(turn_ann.decision_id.is_none());

        // Decision-level: has turn_id and decision_id
        let dec_ann = OTSAnnotation::for_decision("t1", 1, "d1", eval3, 0.9, now);
        assert!(dec_ann.turn_id.is_some());
        assert!(dec_ann.decision_id.is_some());
    }
}
