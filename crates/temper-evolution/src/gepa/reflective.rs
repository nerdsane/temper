//! Reflective dataset construction from OTS trajectories.
//!
//! Converts raw OTS traces into (input, output, feedback) triplets
//! that guide the LLM mutation process. This is the "execution traces
//! as gradients" mechanism from GEPA.

use serde::{Deserialize, Serialize};

/// A reflective triplet extracted from an OTS trajectory.
///
/// Provides the LLM with concrete examples of what happened,
/// what the outcome was, and what feedback to incorporate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReflectiveTriplet {
    /// The input context (what the agent was trying to do).
    pub input: String,

    /// The actual output/outcome (what happened).
    pub output: String,

    /// Feedback signal (what should change).
    pub feedback: String,

    /// Score for this triplet (0.0 = worst, 1.0 = best).
    pub score: f64,

    /// Source trajectory ID.
    pub trajectory_id: String,

    /// Turn number within the trajectory.
    pub turn_id: Option<i32>,

    /// Entity type this triplet relates to.
    pub entity_type: Option<String>,

    /// Action that was attempted.
    pub action: Option<String>,
}

impl ReflectiveTriplet {
    /// Create a new reflective triplet.
    pub fn new(
        input: String,
        output: String,
        feedback: String,
        score: f64,
        trajectory_id: String,
    ) -> Self {
        debug_assert!(
            (0.0..=1.0).contains(&score),
            "Score must be between 0.0 and 1.0, got {}",
            score
        );
        Self {
            input,
            output,
            feedback,
            score,
            trajectory_id,
            turn_id: None,
            entity_type: None,
            action: None,
        }
    }

    /// Set the turn ID.
    pub fn with_turn_id(mut self, turn_id: i32) -> Self {
        self.turn_id = Some(turn_id);
        self
    }

    /// Set the entity type.
    pub fn with_entity_type(mut self, entity_type: String) -> Self {
        self.entity_type = Some(entity_type);
        self
    }

    /// Set the action.
    pub fn with_action(mut self, action: String) -> Self {
        self.action = Some(action);
        self
    }
}

/// A reflective dataset: collection of triplets for a specific evolution target.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReflectiveDataset {
    /// The skill being evolved.
    pub skill_name: String,

    /// Entity type being targeted.
    pub entity_type: String,

    /// Triplets sorted by score (worst first — focus LLM on failures).
    pub triplets: Vec<ReflectiveTriplet>,

    /// Verification errors from previous mutation attempts (if any).
    pub verification_feedback: Vec<String>,
}

impl ReflectiveDataset {
    /// Create a new reflective dataset.
    pub fn new(skill_name: String, entity_type: String) -> Self {
        Self {
            skill_name,
            entity_type,
            triplets: Vec::new(),
            verification_feedback: Vec::new(),
        }
    }

    /// Add a triplet to the dataset.
    pub fn add_triplet(&mut self, triplet: ReflectiveTriplet) {
        self.triplets.push(triplet);
    }

    /// Add verification errors from a previous failed mutation attempt.
    pub fn add_verification_feedback(&mut self, errors: Vec<String>) {
        self.verification_feedback.extend(errors);
    }

    /// Sort triplets by score (worst first) for LLM focus.
    pub fn sort_by_score(&mut self) {
        self.triplets.sort_by(|a, b| {
            a.score
                .partial_cmp(&b.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    /// Get the number of failure triplets (score < 0.5).
    pub fn failure_count(&self) -> usize {
        self.triplets.iter().filter(|t| t.score < 0.5).count()
    }

    /// Get the number of success triplets (score >= 0.5).
    pub fn success_count(&self) -> usize {
        self.triplets.iter().filter(|t| t.score >= 0.5).count()
    }

    /// Format as a prompt context for the LLM mutation step.
    pub fn format_for_llm(&self) -> String {
        let mut out = String::new();

        out.push_str(&format!(
            "# Reflective Dataset for {}/{}\n\n",
            self.skill_name, self.entity_type
        ));

        if !self.verification_feedback.is_empty() {
            out.push_str("## Previous Verification Failures\n\n");
            for (i, err) in self.verification_feedback.iter().enumerate() {
                out.push_str(&format!("{}. {}\n", i + 1, err));
            }
            out.push('\n');
        }

        out.push_str(&format!(
            "## Execution Traces ({} failures, {} successes)\n\n",
            self.failure_count(),
            self.success_count()
        ));

        for (i, triplet) in self.triplets.iter().enumerate() {
            out.push_str(&format!(
                "### Trace {} (score: {:.2})\n",
                i + 1,
                triplet.score
            ));
            if let Some(action) = &triplet.action {
                out.push_str(&format!("**Action**: {}\n", action));
            }
            out.push_str(&format!("**Input**: {}\n", triplet.input));
            out.push_str(&format!("**Output**: {}\n", triplet.output));
            out.push_str(&format!("**Feedback**: {}\n\n", triplet.feedback));
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_triplet_creation() {
        let triplet = ReflectiveTriplet::new(
            "Attempted Reassign on Issue".into(),
            "Error: action not found".into(),
            "Add Reassign action to Issue spec".into(),
            0.0,
            "traj-1".into(),
        )
        .with_turn_id(3)
        .with_entity_type("Issue".into())
        .with_action("Reassign".into());

        assert_eq!(triplet.score, 0.0);
        assert_eq!(triplet.turn_id, Some(3));
        assert_eq!(triplet.action, Some("Reassign".into()));
    }

    #[test]
    fn test_dataset_sorting() {
        let mut dataset = ReflectiveDataset::new("pm".into(), "Issue".into());

        dataset.add_triplet(ReflectiveTriplet::new(
            "a".into(),
            "b".into(),
            "c".into(),
            0.8,
            "t1".into(),
        ));
        dataset.add_triplet(ReflectiveTriplet::new(
            "d".into(),
            "e".into(),
            "f".into(),
            0.2,
            "t2".into(),
        ));
        dataset.add_triplet(ReflectiveTriplet::new(
            "g".into(),
            "h".into(),
            "i".into(),
            0.5,
            "t3".into(),
        ));

        dataset.sort_by_score();

        assert_eq!(dataset.triplets[0].score, 0.2);
        assert_eq!(dataset.triplets[1].score, 0.5);
        assert_eq!(dataset.triplets[2].score, 0.8);
    }

    #[test]
    fn test_dataset_counts() {
        let mut dataset = ReflectiveDataset::new("pm".into(), "Issue".into());

        dataset.add_triplet(ReflectiveTriplet::new(
            "a".into(),
            "b".into(),
            "c".into(),
            0.1,
            "t1".into(),
        ));
        dataset.add_triplet(ReflectiveTriplet::new(
            "d".into(),
            "e".into(),
            "f".into(),
            0.3,
            "t2".into(),
        ));
        dataset.add_triplet(ReflectiveTriplet::new(
            "g".into(),
            "h".into(),
            "i".into(),
            0.9,
            "t3".into(),
        ));

        assert_eq!(dataset.failure_count(), 2);
        assert_eq!(dataset.success_count(), 1);
    }

    #[test]
    fn test_dataset_with_verification_feedback() {
        let mut dataset = ReflectiveDataset::new("pm".into(), "Issue".into());
        dataset.add_verification_feedback(vec![
            "L1: invariant 'assigned_before_work' violated".into(),
            "Counterexample: Open → StartWork without Assign".into(),
        ]);

        let prompt = dataset.format_for_llm();
        assert!(prompt.contains("Previous Verification Failures"));
        assert!(prompt.contains("assigned_before_work"));
    }

    #[test]
    fn test_dataset_serialization() {
        let mut dataset = ReflectiveDataset::new("pm".into(), "Issue".into());
        dataset.add_triplet(ReflectiveTriplet::new(
            "input".into(),
            "output".into(),
            "feedback".into(),
            0.5,
            "traj-1".into(),
        ));

        let json = serde_json::to_string(&dataset).unwrap();
        let parsed: ReflectiveDataset = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.triplets.len(), 1);
        assert_eq!(parsed.skill_name, "pm");
    }
}
