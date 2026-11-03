//! Decision models for agent choices
//!
//! DST adaptations:
//! - `OTSDecision.alternatives` uses `BTreeMap` for deterministic iteration
//! - `OTSDecisionEvaluation.criteria_scores` uses `BTreeMap`
//! - `OTSDecision::new()` uses `sim_uuid()` for ID generation

use crate::models::DecisionType;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use temper_runtime::scheduler::sim_uuid;

/// An alternative action that was considered
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OTSAlternative {
    /// The alternative action
    pub action: String,

    /// Why this alternative was considered
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rationale: Option<String>,

    /// Why this alternative was rejected
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rejected_reason: Option<String>,
}

impl OTSAlternative {
    /// Create a new alternative
    pub fn new(action: impl Into<String>) -> Self {
        Self {
            action: action.into(),
            rationale: None,
            rejected_reason: None,
        }
    }

    /// Set the rationale
    pub fn with_rationale(mut self, rationale: impl Into<String>) -> Self {
        self.rationale = Some(rationale.into());
        self
    }

    /// Set the rejected reason
    pub fn with_rejected_reason(mut self, rejected_reason: impl Into<String>) -> Self {
        self.rejected_reason = Some(rejected_reason.into());
        self
    }
}

/// State at the moment of decision
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OTSDecisionState {
    /// Summary of context at decision time
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_summary: Option<String>,

    /// Actions available to agent
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_actions: Vec<String>,
}

impl Default for OTSDecisionState {
    fn default() -> Self {
        Self::new()
    }
}

impl OTSDecisionState {
    /// Create a new empty decision state
    pub fn new() -> Self {
        Self {
            context_summary: None,
            available_actions: Vec::new(),
        }
    }

    /// Set the context summary
    pub fn with_context_summary(mut self, context_summary: impl Into<String>) -> Self {
        self.context_summary = Some(context_summary.into());
        self
    }

    /// Add an available action
    pub fn with_action(mut self, action: impl Into<String>) -> Self {
        self.available_actions.push(action.into());
        self
    }
}

/// The chosen action
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OTSChoice {
    /// The chosen action
    pub action: String,

    /// Arguments for the action
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<serde_json::Value>,

    /// Rationale for choosing this action
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rationale: Option<String>,

    /// Confidence in this choice (0.0 to 1.0)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
}

impl OTSChoice {
    /// Create a new choice with the given action
    pub fn new(action: impl Into<String>) -> Self {
        Self {
            action: action.into(),
            arguments: None,
            rationale: None,
            confidence: None,
        }
    }

    /// Set the arguments
    pub fn with_arguments(mut self, arguments: serde_json::Value) -> Self {
        self.arguments = Some(arguments);
        self
    }

    /// Set the rationale
    pub fn with_rationale(mut self, rationale: impl Into<String>) -> Self {
        self.rationale = Some(rationale.into());
        self
    }

    /// Set the confidence (must be between 0.0 and 1.0)
    pub fn with_confidence(mut self, confidence: f64) -> Self {
        assert!(
            (0.0..=1.0).contains(&confidence),
            "Confidence must be between 0.0 and 1.0, got {}",
            confidence
        );
        self.confidence = Some(confidence);
        self
    }
}

/// Consequence of a decision
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OTSConsequence {
    /// Whether the action succeeded
    pub success: bool,

    /// Summary of the result
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_summary: Option<String>,

    /// Type of error if it failed
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_type: Option<String>,
}

impl OTSConsequence {
    /// Create a successful consequence
    pub fn success() -> Self {
        Self {
            success: true,
            result_summary: None,
            error_type: None,
        }
    }

    /// Create a failed consequence
    pub fn failure() -> Self {
        Self {
            success: false,
            result_summary: None,
            error_type: None,
        }
    }

    /// Set the result summary
    pub fn with_result_summary(mut self, result_summary: impl Into<String>) -> Self {
        self.result_summary = Some(result_summary.into());
        self
    }

    /// Set the error type
    pub fn with_error_type(mut self, error_type: impl Into<String>) -> Self {
        self.error_type = Some(error_type.into());
        self
    }
}

/// Counterfactual analysis
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OTSCounterfactual {
    /// What would have been a better alternative
    #[serde(skip_serializing_if = "Option::is_none")]
    pub better_alternative: Option<String>,

    /// Estimated improvement if better alternative was chosen
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_improvement: Option<f64>,
}

impl Default for OTSCounterfactual {
    fn default() -> Self {
        Self::new()
    }
}

impl OTSCounterfactual {
    /// Create a new empty counterfactual
    pub fn new() -> Self {
        Self {
            better_alternative: None,
            estimated_improvement: None,
        }
    }

    /// Set the better alternative
    pub fn with_better_alternative(mut self, better_alternative: impl Into<String>) -> Self {
        self.better_alternative = Some(better_alternative.into());
        self
    }

    /// Set the estimated improvement
    pub fn with_estimated_improvement(mut self, estimated_improvement: f64) -> Self {
        self.estimated_improvement = Some(estimated_improvement);
        self
    }
}

/// Evaluation of a decision
///
/// DST adaptation: `criteria_scores` uses `BTreeMap` for deterministic iteration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OTSDecisionEvaluation {
    /// ID of the evaluator
    pub evaluator_id: String,

    /// Overall score (0.0 to 1.0)
    pub score: f64,

    /// Scores for individual criteria (BTreeMap for deterministic iteration)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub criteria_scores: Option<BTreeMap<String, f64>>,

    /// Feedback text
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback: Option<String>,

    /// Counterfactual analysis
    #[serde(skip_serializing_if = "Option::is_none")]
    pub counterfactual: Option<OTSCounterfactual>,
}

impl OTSDecisionEvaluation {
    /// Create a new evaluation with the given evaluator and score
    pub fn new(evaluator_id: impl Into<String>, score: f64) -> Self {
        assert!(
            (0.0..=1.0).contains(&score),
            "Score must be between 0.0 and 1.0, got {}",
            score
        );
        Self {
            evaluator_id: evaluator_id.into(),
            score,
            criteria_scores: None,
            feedback: None,
            counterfactual: None,
        }
    }

    /// Set the criteria scores
    pub fn with_criteria_scores(mut self, criteria_scores: BTreeMap<String, f64>) -> Self {
        self.criteria_scores = Some(criteria_scores);
        self
    }

    /// Set the feedback
    pub fn with_feedback(mut self, feedback: impl Into<String>) -> Self {
        self.feedback = Some(feedback.into());
        self
    }

    /// Set the counterfactual
    pub fn with_counterfactual(mut self, counterfactual: OTSCounterfactual) -> Self {
        self.counterfactual = Some(counterfactual);
        self
    }
}

/// Credit assignment for a decision
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OTSCreditAssignment {
    /// Contribution to outcome (-1.0 to 1.0)
    /// Serialized as "impact" for compatibility
    #[serde(rename = "impact")]
    pub contribution_to_outcome: f64,

    /// Whether this decision was pivotal
    #[serde(default)]
    pub pivotal: bool,

    /// Explanation of credit assignment
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explanation: Option<String>,
}

impl OTSCreditAssignment {
    /// Create a new credit assignment with the given contribution
    pub fn new(contribution_to_outcome: f64) -> Self {
        assert!(
            (-1.0..=1.0).contains(&contribution_to_outcome),
            "Contribution must be between -1.0 and 1.0, got {}",
            contribution_to_outcome
        );
        Self {
            contribution_to_outcome,
            pivotal: false,
            explanation: None,
        }
    }

    /// Mark this decision as pivotal
    pub fn with_pivotal(mut self, pivotal: bool) -> Self {
        self.pivotal = pivotal;
        self
    }

    /// Set the explanation
    pub fn with_explanation(mut self, explanation: impl Into<String>) -> Self {
        self.explanation = Some(explanation.into());
        self
    }
}

/// An atomic decision point within a turn
///
/// Captures: state -> alternatives -> choice -> consequence
///
/// DST adaptations:
/// - `alternatives` uses `BTreeMap` for deterministic iteration
/// - `decision_id` generated via `sim_uuid()`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OTSDecision {
    /// Unique decision identifier
    pub decision_id: String,

    /// Type of decision
    pub decision_type: DecisionType,

    /// State at decision time
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<OTSDecisionState>,

    /// Alternatives considered (grouped by category, BTreeMap for deterministic iteration)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alternatives: Option<BTreeMap<String, Vec<OTSAlternative>>>,

    /// The chosen action
    pub choice: OTSChoice,

    /// Consequence of the choice
    pub consequence: OTSConsequence,

    /// Evaluation of the decision
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evaluation: Option<OTSDecisionEvaluation>,

    /// Credit assignment for this decision
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credit_assignment: Option<OTSCreditAssignment>,

    /// Optional embedding vector for similarity search
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding: Option<Vec<f32>>,

    /// The `tool_call` id that links this decision to the observation it caused.
    ///
    /// Without it a decision and the tool result that followed can only be
    /// paired by position, which breaks under interleaved or parallel tool
    /// calls. Carries the provider's tool-call id verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause_id: Option<String>,
}

impl OTSDecision {
    /// Create a new decision with the given type, choice, and consequence.
    ///
    /// Uses `sim_uuid()` for deterministic ID generation in simulation.
    pub fn new(
        decision_type: DecisionType,
        choice: OTSChoice,
        consequence: OTSConsequence,
    ) -> Self {
        Self {
            decision_id: sim_uuid().to_string(),
            decision_type,
            state: None,
            alternatives: None,
            choice,
            consequence,
            evaluation: None,
            credit_assignment: None,
            embedding: None,
            cause_id: None,
        }
    }

    /// Set the decision ID
    pub fn with_decision_id(mut self, decision_id: impl Into<String>) -> Self {
        self.decision_id = decision_id.into();
        self
    }

    /// Set the state
    pub fn with_state(mut self, state: OTSDecisionState) -> Self {
        self.state = Some(state);
        self
    }

    /// Add alternatives in a category
    pub fn with_alternatives(
        mut self,
        category: impl Into<String>,
        alternatives: Vec<OTSAlternative>,
    ) -> Self {
        self.alternatives
            .get_or_insert_with(BTreeMap::new)
            .insert(category.into(), alternatives);
        self
    }

    /// Set the evaluation
    pub fn with_evaluation(mut self, evaluation: OTSDecisionEvaluation) -> Self {
        self.evaluation = Some(evaluation);
        self
    }

    /// Set the credit assignment
    pub fn with_credit_assignment(mut self, credit_assignment: OTSCreditAssignment) -> Self {
        self.credit_assignment = Some(credit_assignment);
        self
    }

    /// Set the embedding vector
    pub fn with_embedding(mut self, embedding: Vec<f32>) -> Self {
        self.embedding = Some(embedding);
        self
    }

    /// Set the causing `tool_call` id, linking this decision to its observation
    pub fn with_cause_id(mut self, cause_id: impl Into<String>) -> Self {
        self.cause_id = Some(cause_id.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[should_panic(expected = "Confidence must be between 0.0 and 1.0")]
    fn test_choice_invalid_confidence() {
        OTSChoice::new("test").with_confidence(1.5);
    }

    #[test]
    #[should_panic(expected = "Score must be between 0.0 and 1.0")]
    fn test_evaluation_invalid_score() {
        OTSDecisionEvaluation::new("test", 2.0);
    }

    #[test]
    #[should_panic(expected = "Contribution must be between -1.0 and 1.0")]
    fn test_credit_assignment_invalid_contribution() {
        OTSCreditAssignment::new(1.5);
    }
}
