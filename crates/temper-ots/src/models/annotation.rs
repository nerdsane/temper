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
    fn test_evaluator_serialization() {
        let evaluator = OTSEvaluator::new("eval_123", EvaluatorType::Human).with_version("1.0");

        let json_str = serde_json::to_string(&evaluator).unwrap();
        let parsed: OTSEvaluator = serde_json::from_str(&json_str).unwrap();

        assert_eq!(parsed.id, "eval_123");
        assert_eq!(parsed.evaluator_type, EvaluatorType::Human);
        assert_eq!(parsed.version, Some("1.0".to_string()));

        // Check that "type" is used in JSON
        assert!(json_str.contains("\"type\":\"human\""));
    }

    #[test]
    fn test_evaluator_without_version() {
        let evaluator = OTSEvaluator::new("eval_456", EvaluatorType::Model);
        let json_str = serde_json::to_string(&evaluator).unwrap();

        // Version should not appear
        assert!(!json_str.contains("\"version\""));
    }

    #[test]
    fn test_trajectory_level_annotation() {
        let now = sim_now();
        let evaluator = OTSEvaluator::new("human_eval", EvaluatorType::Human);
        let annotation = OTSAnnotation::new("traj_123", evaluator, 0.85, now)
            .with_label("good_execution")
            .with_feedback("Clear reasoning");

        assert_eq!(annotation.trajectory_id, "traj_123");
        assert_eq!(annotation.turn_id, None);
        assert_eq!(annotation.decision_id, None);
        assert_eq!(annotation.score, 0.85);

        let json_str = serde_json::to_string(&annotation).unwrap();
        let parsed: OTSAnnotation = serde_json::from_str(&json_str).unwrap();

        assert_eq!(parsed.trajectory_id, "traj_123");
        assert_eq!(parsed.score, 0.85);
        assert_eq!(parsed.label, Some("good_execution".to_string()));
    }

    #[test]
    fn test_turn_level_annotation() {
        let now = sim_now();
        let evaluator = OTSEvaluator::new("model_eval", EvaluatorType::Model);
        let annotation = OTSAnnotation::for_turn("traj_456", 2, evaluator, 0.92, now);

        assert_eq!(annotation.trajectory_id, "traj_456");
        assert_eq!(annotation.turn_id, Some(2));
        assert_eq!(annotation.decision_id, None);

        let json_str = serde_json::to_string(&annotation).unwrap();
        let parsed: OTSAnnotation = serde_json::from_str(&json_str).unwrap();

        assert_eq!(parsed.turn_id, Some(2));
    }

    #[test]
    fn test_decision_level_annotation() {
        let now = sim_now();
        let evaluator = OTSEvaluator::new("heuristic_eval", EvaluatorType::Heuristic);
        let annotation =
            OTSAnnotation::for_decision("traj_789", 3, "decision_abc", evaluator, 0.75, now)
                .with_feedback("Could be optimized");

        assert_eq!(annotation.trajectory_id, "traj_789");
        assert_eq!(annotation.turn_id, Some(3));
        assert_eq!(annotation.decision_id, Some("decision_abc".to_string()));
        assert_eq!(annotation.score, 0.75);

        let json_str = serde_json::to_string(&annotation).unwrap();
        let parsed: OTSAnnotation = serde_json::from_str(&json_str).unwrap();

        assert_eq!(parsed.decision_id, Some("decision_abc".to_string()));
        assert_eq!(parsed.feedback, Some("Could be optimized".to_string()));
    }

    #[test]
    #[should_panic(expected = "Score must be between 0.0 and 1.0")]
    fn test_annotation_invalid_score() {
        let now = sim_now();
        let evaluator = OTSEvaluator::new("test", EvaluatorType::Human);
        OTSAnnotation::new("traj", evaluator, 1.5, now);
    }

    #[test]
    fn test_annotation_minimal() {
        let now = sim_now();
        let evaluator = OTSEvaluator::new("eval", EvaluatorType::Model);
        let annotation = OTSAnnotation::new("traj_minimal", evaluator, 0.5, now);

        let json_str = serde_json::to_string(&annotation).unwrap();

        // Optional fields should not appear
        assert!(!json_str.contains("\"turn_id\""));
        assert!(!json_str.contains("\"decision_id\""));
        assert!(!json_str.contains("\"label\""));
        assert!(!json_str.contains("\"feedback\""));
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
