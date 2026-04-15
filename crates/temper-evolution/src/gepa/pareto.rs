//! Pareto frontier management for multi-objective optimization.
//!
//! The Pareto frontier tracks the set of non-dominated candidates.
//! A candidate dominates another if it is at least as good on all
//! objectives and strictly better on at least one.

use super::candidate::Candidate;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// The Pareto frontier: set of non-dominated candidates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParetoFrontier {
    /// Members indexed by candidate ID.
    pub members: BTreeMap<String, Candidate>,
}

/// Mapping from frontier key (objective, instance, or hybrid key) to
/// candidate IDs that currently support that key's frontier.
///
/// This mirrors GEPA's frontier-support representation where a candidate can
/// be in multiple local frontiers and selection is based on support frequency.
pub type FrontierMapping = BTreeMap<String, BTreeSet<String>>;

/// Aggregate score lookup by candidate ID.
pub type AggregateScores = BTreeMap<String, f64>;

impl ParetoFrontier {
    /// Create an empty Pareto frontier.
    pub fn new() -> Self {
        Self {
            members: BTreeMap::new(),
        }
    }

    /// Check if candidate `a` dominates candidate `b`.
    ///
    /// Domination: `a` is at least as good on all objectives AND
    /// strictly better on at least one.
    pub fn dominates(a_scores: &BTreeMap<String, f64>, b_scores: &BTreeMap<String, f64>) -> bool {
        if a_scores.is_empty() || b_scores.is_empty() {
            return false;
        }

        // Collect all objective keys from both sides.
        let all_keys: std::collections::BTreeSet<&String> =
            a_scores.keys().chain(b_scores.keys()).collect();

        let mut at_least_as_good = true;
        let mut strictly_better = false;

        for key in all_keys {
            let a_val = a_scores.get(key).copied().unwrap_or(0.0);
            let b_val = b_scores.get(key).copied().unwrap_or(0.0);

            if a_val < b_val {
                at_least_as_good = false;
                break;
            }
            if a_val > b_val {
                strictly_better = true;
            }
        }

        at_least_as_good && strictly_better
    }

    /// Try to add a candidate to the frontier.
    ///
    /// Returns `true` if the candidate was added (is non-dominated).
    /// Removes any existing members that the new candidate dominates.
    pub fn try_add(&mut self, candidate: Candidate) -> bool {
        let new_scores = &candidate.scores;

        // Check if any existing member dominates the new candidate
        for existing in self.members.values() {
            if Self::dominates(&existing.scores, new_scores) {
                return false;
            }
        }

        // Remove members that the new candidate dominates
        let dominated: Vec<String> = self
            .members
            .iter()
            .filter(|(_, existing)| Self::dominates(new_scores, &existing.scores))
            .map(|(id, _)| id.clone())
            .collect();

        for id in dominated {
            self.members.remove(&id);
        }

        self.members.insert(candidate.id.clone(), candidate);
        true
    }

    /// Get the number of members in the frontier.
    pub fn len(&self) -> usize {
        self.members.len()
    }

    /// Check if the frontier is empty.
    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// Select the candidate with the worst score on a given objective.
    ///
    /// Used to identify the weakest member for targeted improvement.
    pub fn weakest_on(&self, objective: &str) -> Option<&Candidate> {
        self.members
            .values()
            .filter(|c| c.scores.contains_key(objective))
            .min_by(|a, b| {
                let a_score = a.scores[objective];
                let b_score = b.scores[objective];
                a_score
                    .partial_cmp(&b_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    }

    /// Get all members as a sorted vec (by ID for determinism).
    pub fn members_sorted(&self) -> Vec<&Candidate> {
        self.members.values().collect()
    }

    /// Remove dominated candidates from a frontier-support mapping.
    ///
    /// A candidate is considered dominated if, for every frontier key where it
    /// appears, there exists at least one other surviving candidate in that same
    /// frontier key. This is the Rust analogue of GEPA's
    /// `remove_dominated_programs`.
    pub fn remove_dominated_programs(
        mapping: &FrontierMapping,
        aggregate_scores: &AggregateScores,
    ) -> FrontierMapping {
        let mut freq: BTreeMap<String, usize> = BTreeMap::new();
        for front in mapping.values() {
            for candidate_id in front {
                *freq.entry(candidate_id.clone()).or_insert(0) += 1;
            }
        }

        let mut programs: Vec<String> = freq.keys().cloned().collect();
        programs.sort_by(|a, b| {
            let a_score = aggregate_scores.get(a).copied().unwrap_or(0.0);
            let b_score = aggregate_scores.get(b).copied().unwrap_or(0.0);
            a_score
                .partial_cmp(&b_score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.cmp(b))
        });

        let mut dominated: BTreeSet<String> = BTreeSet::new();
        let mut changed = true;
        while changed {
            changed = false;
            for y in &programs {
                if dominated.contains(y) {
                    continue;
                }

                let others: BTreeSet<String> = programs
                    .iter()
                    .filter(|p| *p != y && !dominated.contains(*p))
                    .cloned()
                    .collect();

                if Self::is_dominated_in_mapping(y, &others, mapping) {
                    dominated.insert(y.clone());
                    changed = true;
                    break;
                }
            }
        }

        let dominators: BTreeSet<String> = programs
            .into_iter()
            .filter(|p| !dominated.contains(p))
            .collect();

        let mut reduced = FrontierMapping::new();
        for (key, front) in mapping {
            let filtered: BTreeSet<String> = front
                .iter()
                .filter(|candidate_id| dominators.contains(*candidate_id))
                .cloned()
                .collect();
            if !filtered.is_empty() {
                reduced.insert(key.clone(), filtered);
            }
        }

        reduced
    }

    /// Return all non-dominated candidate IDs for a frontier-support mapping.
    pub fn find_dominator_programs(
        mapping: &FrontierMapping,
        aggregate_scores: &AggregateScores,
    ) -> BTreeSet<String> {
        let reduced = Self::remove_dominated_programs(mapping, aggregate_scores);
        reduced
            .values()
            .flat_map(|front| front.iter().cloned())
            .collect()
    }

    /// Select a candidate from the reduced frontier mapping using support
    /// frequency first, then aggregate score, then stable lexical tie-break.
    ///
    /// Upstream GEPA samples proportionally to support frequency. We keep this
    /// deterministic for reproducible simulation by choosing the maximal
    /// `(frequency, aggregate_score, candidate_id)` tuple.
    pub fn select_candidate_from_frontier(
        mapping: &FrontierMapping,
        aggregate_scores: &AggregateScores,
    ) -> Option<String> {
        let reduced = Self::remove_dominated_programs(mapping, aggregate_scores);
        if reduced.is_empty() {
            return None;
        }

        let mut frequency: BTreeMap<String, usize> = BTreeMap::new();
        for front in reduced.values() {
            for candidate_id in front {
                *frequency.entry(candidate_id.clone()).or_insert(0) += 1;
            }
        }

        frequency
            .into_iter()
            .max_by(|(id_a, freq_a), (id_b, freq_b)| {
                freq_a
                    .cmp(freq_b)
                    .then_with(|| {
                        let score_a = aggregate_scores.get(id_a).copied().unwrap_or(0.0);
                        let score_b = aggregate_scores.get(id_b).copied().unwrap_or(0.0);
                        score_a
                            .partial_cmp(&score_b)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .then_with(|| id_b.cmp(id_a))
            })
            .map(|(id, _)| id)
    }

    fn is_dominated_in_mapping(
        candidate_id: &str,
        other_candidates: &BTreeSet<String>,
        mapping: &FrontierMapping,
    ) -> bool {
        let fronts_for_candidate: Vec<&BTreeSet<String>> = mapping
            .values()
            .filter(|front| front.contains(candidate_id))
            .collect();

        if fronts_for_candidate.is_empty() {
            return false;
        }

        for front in fronts_for_candidate {
            let found_dominator = front.iter().any(|other| other_candidates.contains(other));
            if !found_dominator {
                return false;
            }
        }
        true
    }
}

impl Default for ParetoFrontier {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn make_candidate(id: &str, scores: &[(&str, f64)]) -> Candidate {
        let mut c = Candidate::new(
            id.into(),
            "spec".into(),
            "pm".into(),
            "Issue".into(),
            1,
            Utc::now(),
        );
        for (obj, score) in scores {
            c.set_score((*obj).into(), *score);
        }
        c
    }

    #[test]
    fn test_dominance_basic() {
        let a = BTreeMap::from([("x".into(), 0.9), ("y".into(), 0.8)]);
        let b = BTreeMap::from([("x".into(), 0.7), ("y".into(), 0.6)]);

        assert!(ParetoFrontier::dominates(&a, &b));
        assert!(!ParetoFrontier::dominates(&b, &a));
    }

    #[test]
    fn test_dominance_equal() {
        let a = BTreeMap::from([("x".into(), 0.9), ("y".into(), 0.8)]);
        let b = BTreeMap::from([("x".into(), 0.9), ("y".into(), 0.8)]);

        // Equal scores: neither dominates
        assert!(!ParetoFrontier::dominates(&a, &b));
        assert!(!ParetoFrontier::dominates(&b, &a));
    }

    #[test]
    fn test_dominance_tradeoff() {
        let a = BTreeMap::from([("x".into(), 0.9), ("y".into(), 0.5)]);
        let b = BTreeMap::from([("x".into(), 0.5), ("y".into(), 0.9)]);

        // Trade-off: neither dominates
        assert!(!ParetoFrontier::dominates(&a, &b));
        assert!(!ParetoFrontier::dominates(&b, &a));
    }

    #[test]
    fn test_dominance_empty_scores() {
        let empty = BTreeMap::new();
        let non_empty = BTreeMap::from([("x".into(), 0.9)]);

        assert!(!ParetoFrontier::dominates(&empty, &non_empty));
        assert!(!ParetoFrontier::dominates(&non_empty, &empty));
    }

    #[test]
    fn test_frontier_add_non_dominated() {
        let mut frontier = ParetoFrontier::new();

        let c1 = make_candidate("c1", &[("x", 0.9), ("y", 0.5)]);
        let c2 = make_candidate("c2", &[("x", 0.5), ("y", 0.9)]);

        assert!(frontier.try_add(c1));
        assert!(frontier.try_add(c2));
        assert_eq!(frontier.len(), 2);
    }

    #[test]
    fn test_frontier_dominated_rejected() {
        let mut frontier = ParetoFrontier::new();

        let c1 = make_candidate("c1", &[("x", 0.9), ("y", 0.8)]);
        let c2 = make_candidate("c2", &[("x", 0.7), ("y", 0.6)]);

        assert!(frontier.try_add(c1));
        assert!(!frontier.try_add(c2)); // c2 dominated by c1
        assert_eq!(frontier.len(), 1);
    }

    #[test]
    fn test_frontier_new_dominates_existing() {
        let mut frontier = ParetoFrontier::new();

        let c1 = make_candidate("c1", &[("x", 0.7), ("y", 0.6)]);
        let c2 = make_candidate("c2", &[("x", 0.9), ("y", 0.8)]);

        assert!(frontier.try_add(c1));
        assert!(frontier.try_add(c2)); // c2 dominates c1, c1 removed
        assert_eq!(frontier.len(), 1);
        assert!(frontier.members.contains_key("c2"));
    }

    #[test]
    fn test_frontier_weakest_on() {
        let mut frontier = ParetoFrontier::new();

        let c1 = make_candidate("c1", &[("x", 0.9), ("y", 0.3)]);
        let c2 = make_candidate("c2", &[("x", 0.3), ("y", 0.9)]);

        frontier.try_add(c1);
        frontier.try_add(c2);

        let weakest_x = frontier.weakest_on("x").unwrap();
        assert_eq!(weakest_x.id, "c2");

        let weakest_y = frontier.weakest_on("y").unwrap();
        assert_eq!(weakest_y.id, "c1");
    }

    #[test]
    fn test_frontier_serialization() {
        let mut frontier = ParetoFrontier::new();
        frontier.try_add(make_candidate("c1", &[("x", 0.8)]));

        let json = serde_json::to_string(&frontier).unwrap();
        let parsed: ParetoFrontier = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.len(), 1);
    }

    #[test]
    fn test_remove_dominated_programs_matches_frequency_frontier_intuition() {
        // p1 is present in every front but always co-present with stronger peers,
        // so it should be removed as dominated support.
        let mapping = FrontierMapping::from([
            (
                "a".into(),
                BTreeSet::from(["p1".to_string(), "p2".to_string()]),
            ),
            (
                "b".into(),
                BTreeSet::from(["p1".to_string(), "p3".to_string()]),
            ),
            (
                "c".into(),
                BTreeSet::from(["p1".to_string(), "p2".to_string(), "p3".to_string()]),
            ),
        ]);

        let scores =
            AggregateScores::from([("p1".into(), 0.3), ("p2".into(), 0.9), ("p3".into(), 0.8)]);

        let reduced = ParetoFrontier::remove_dominated_programs(&mapping, &scores);
        let survivors: BTreeSet<String> = reduced
            .values()
            .flat_map(|front| front.iter().cloned())
            .collect();
        assert!(!survivors.contains("p1"));
        assert!(survivors.contains("p2"));
        assert!(survivors.contains("p3"));
    }

    #[test]
    fn test_select_candidate_from_frontier_prefers_support_then_score() {
        let mapping = FrontierMapping::from([
            (
                "x".into(),
                BTreeSet::from(["c1".to_string(), "c2".to_string()]),
            ),
            ("y".into(), BTreeSet::from(["c1".to_string()])),
            ("z".into(), BTreeSet::from(["c3".to_string()])),
        ]);
        let scores =
            AggregateScores::from([("c1".into(), 0.7), ("c2".into(), 0.95), ("c3".into(), 0.5)]);

        // c1 has highest support frequency (2 fronts), so it should be selected
        // even though c2 has higher aggregate score.
        let selected = ParetoFrontier::select_candidate_from_frontier(&mapping, &scores)
            .expect("candidate should be selected");
        assert_eq!(selected, "c1");
    }
}
