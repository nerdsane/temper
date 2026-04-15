//! Multi-objective scoring for GEPA candidates.
//!
//! Scores are computed from replay results and other signals.
//! Each score is a value between 0.0 and 1.0.

use super::replay::ReplayResult;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Configuration for the scoring system.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoringConfig {
    /// Weights for each objective (objective_name → weight).
    /// Weights are used for weighted-sum aggregation when needed.
    pub weights: BTreeMap<String, f64>,
}

impl Default for ScoringConfig {
    fn default() -> Self {
        let mut weights = BTreeMap::new();
        weights.insert("success_rate".into(), 1.0);
        weights.insert("coverage".into(), 0.8);
        weights.insert("guard_pass_rate".into(), 0.6);
        Self { weights }
    }
}

/// Multi-objective scores for a candidate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObjectiveScores {
    /// Individual objective scores (objective_name → score 0.0-1.0).
    pub scores: BTreeMap<String, f64>,
}

impl ObjectiveScores {
    /// Compute scores from a replay result.
    pub fn from_replay(result: &ReplayResult) -> Self {
        let mut scores = BTreeMap::new();

        // Success rate: fraction of attempted actions that succeeded
        if result.actions_attempted > 0 {
            scores.insert(
                "success_rate".into(),
                result.succeeded as f64 / result.actions_attempted as f64,
            );
        }

        // Guard pass rate: 1.0 - (guard rejections / attempted)
        if result.actions_attempted > 0 {
            scores.insert(
                "guard_pass_rate".into(),
                1.0 - (result.guard_rejections as f64 / result.actions_attempted as f64),
            );
        }

        // Coverage: fraction of actions that are known (not unknown)
        if result.actions_attempted > 0 {
            scores.insert(
                "coverage".into(),
                1.0 - (result.unknown_actions as f64 / result.actions_attempted as f64),
            );
        }

        Self { scores }
    }

    /// Compute weighted sum using the given config.
    pub fn weighted_sum(&self, config: &ScoringConfig) -> f64 {
        let mut total = 0.0;
        let mut weight_sum = 0.0;

        for (objective, weight) in &config.weights {
            if let Some(score) = self.scores.get(objective) {
                total += score * weight;
                weight_sum += weight;
            }
        }

        if weight_sum > 0.0 {
            total / weight_sum
        } else {
            0.0
        }
    }

    /// Convert to a BTreeMap for storage on a Candidate.
    pub fn into_map(self) -> BTreeMap<String, f64> {
        self.scores
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scores_from_replay_perfect() {
        let result = ReplayResult {
            actions_attempted: 10,
            succeeded: 10,
            guard_rejections: 0,
            unknown_actions: 0,
            invalid_transitions: 0,
            errors: Vec::new(),
        };

        let scores = ObjectiveScores::from_replay(&result);
        assert_eq!(scores.scores["success_rate"], 1.0);
        assert_eq!(scores.scores["guard_pass_rate"], 1.0);
        assert_eq!(scores.scores["coverage"], 1.0);
    }

    #[test]
    fn test_scores_from_replay_partial() {
        let result = ReplayResult {
            actions_attempted: 10,
            succeeded: 7,
            guard_rejections: 2,
            unknown_actions: 1,
            invalid_transitions: 0,
            errors: Vec::new(),
        };

        let scores = ObjectiveScores::from_replay(&result);
        assert!((scores.scores["success_rate"] - 0.7).abs() < f64::EPSILON);
        assert!((scores.scores["guard_pass_rate"] - 0.8).abs() < f64::EPSILON);
        assert!((scores.scores["coverage"] - 0.9).abs() < f64::EPSILON);
    }

    #[test]
    fn test_scores_from_replay_empty() {
        let result = ReplayResult {
            actions_attempted: 0,
            succeeded: 0,
            guard_rejections: 0,
            unknown_actions: 0,
            invalid_transitions: 0,
            errors: Vec::new(),
        };

        let scores = ObjectiveScores::from_replay(&result);
        assert!(scores.scores.is_empty());
    }

    #[test]
    fn test_weighted_sum() {
        let scores = ObjectiveScores {
            scores: BTreeMap::from([
                ("success_rate".into(), 0.8),
                ("coverage".into(), 0.6),
                ("guard_pass_rate".into(), 1.0),
            ]),
        };

        let config = ScoringConfig::default();
        let sum = scores.weighted_sum(&config);

        // (0.8*1.0 + 0.6*0.8 + 1.0*0.6) / (1.0 + 0.8 + 0.6) = 1.88 / 2.4
        let expected = (0.8 * 1.0 + 0.6 * 0.8 + 1.0 * 0.6) / (1.0 + 0.8 + 0.6);
        assert!((sum - expected).abs() < f64::EPSILON);
    }

    #[test]
    fn test_scoring_config_default() {
        let config = ScoringConfig::default();
        assert_eq!(config.weights.len(), 3);
        assert!(config.weights.contains_key("success_rate"));
    }
}
