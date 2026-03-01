//! Post-hoc path extraction from TemperModel.
//!
//! BFS on the status graph to extract shortest paths to target states.
//! Operates on status strings only (not full state space) for efficiency.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::Serialize;

use crate::model::TemperModel;

/// A single step in a reachable path.
#[derive(Debug, Clone, Serialize)]
pub struct PathStep {
    /// The status at this step.
    pub state: String,
    /// The action taken to reach the next step (None for the initial state).
    pub action: Option<String>,
}

/// A complete path from initial state to a target state.
#[derive(Debug, Clone, Serialize)]
pub struct ReachablePath {
    /// Steps from initial state to target.
    pub steps: Vec<PathStep>,
    /// Number of transitions (steps - 1).
    pub length: usize,
}

/// Configuration for path extraction.
#[derive(Debug, Clone)]
pub struct PathExtractionConfig {
    /// Target states to find paths to. Empty = all terminal states.
    pub target_states: Vec<String>,
    /// Maximum number of paths per target state.
    pub max_paths_per_target: usize,
    /// Maximum path length (in transitions).
    pub max_path_length: usize,
}

impl Default for PathExtractionConfig {
    fn default() -> Self {
        Self {
            target_states: Vec::new(),
            max_paths_per_target: 5,
            max_path_length: 20,
        }
    }
}

/// Result of path extraction.
#[derive(Debug, Clone, Serialize)]
pub struct PathExtractionResult {
    /// Shortest paths grouped by target state.
    pub paths_by_target: BTreeMap<String, Vec<ReachablePath>>,
    /// States that are unreachable from any initial state.
    pub unreachable_states: Vec<String>,
    /// Terminal states (no outgoing transitions).
    pub terminal_states: Vec<String>,
    /// Total states visited during BFS.
    pub states_visited: usize,
}

/// Extract shortest paths from the model's initial state to target states via BFS.
///
/// Builds an adjacency list from the model's transitions (ignoring guards/effects),
/// then runs BFS from the initial status to find shortest paths to each reachable state.
pub fn extract_paths(model: &TemperModel, config: &PathExtractionConfig) -> PathExtractionResult {
    // Step 1: Build adjacency list from transitions.
    // Each from_status maps to a list of (action_name, to_status) edges.
    let mut adjacency: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut all_from_states: BTreeSet<String> = BTreeSet::new();

    for transition in &model.transitions {
        for from_status in &transition.from_states {
            let to_status = transition
                .to_state
                .clone()
                .unwrap_or_else(|| from_status.clone());
            adjacency
                .entry(from_status.clone())
                .or_default()
                .push((transition.name.clone(), to_status));
            all_from_states.insert(from_status.clone());
        }
    }

    // Step 2: Find terminal states (states that never appear as a source).
    let terminal_states: Vec<String> = model
        .states
        .iter()
        .filter(|s| !all_from_states.contains(*s))
        .cloned()
        .collect();

    // Step 3: BFS from initial_status with parent tracking.
    let initial = &model.initial_status;
    // parent map: state -> (parent_state, action_name)
    let mut parent: BTreeMap<String, (String, String)> = BTreeMap::new();
    let mut visited: BTreeSet<String> = BTreeSet::new();
    let mut queue: VecDeque<(String, usize)> = VecDeque::new();

    visited.insert(initial.clone());
    queue.push_back((initial.clone(), 0));

    while let Some((current, depth)) = queue.pop_front() {
        if depth >= config.max_path_length {
            continue;
        }

        if let Some(edges) = adjacency.get(&current) {
            for (action, to_status) in edges {
                if !visited.contains(to_status) {
                    visited.insert(to_status.clone());
                    parent.insert(to_status.clone(), (current.clone(), action.clone()));
                    queue.push_back((to_status.clone(), depth + 1));
                }
            }
        }
    }

    let states_visited = visited.len();

    // Step 4: Determine targets.
    let targets: Vec<String> = if config.target_states.is_empty() {
        // Default to all terminal states.
        terminal_states.clone()
    } else {
        config.target_states.clone()
    };

    // Step 5: Reconstruct paths by backtracking from target to initial.
    let mut paths_by_target: BTreeMap<String, Vec<ReachablePath>> = BTreeMap::new();

    for target in &targets {
        if !visited.contains(target) {
            continue;
        }
        if target == initial && !config.target_states.contains(target) {
            // Skip initial state unless explicitly requested as a target.
            continue;
        }

        let path = reconstruct_path(initial, target, &parent);
        if let Some(path) = path {
            paths_by_target
                .entry(target.clone())
                .or_default()
                .push(path);
        }
    }

    // Step 6: Find unreachable states.
    let unreachable_states: Vec<String> = model
        .states
        .iter()
        .filter(|s| !visited.contains(*s))
        .cloned()
        .collect();

    PathExtractionResult {
        paths_by_target,
        unreachable_states,
        terminal_states,
        states_visited,
    }
}

/// Reconstruct a path from initial to target by following the parent map backwards.
fn reconstruct_path(
    initial: &str,
    target: &str,
    parent: &BTreeMap<String, (String, String)>,
) -> Option<ReachablePath> {
    if initial == target {
        return Some(ReachablePath {
            steps: vec![PathStep {
                state: initial.to_string(),
                action: None,
            }],
            length: 0,
        });
    }

    // Backtrack from target to initial.
    let mut reverse_steps: Vec<(String, String)> = Vec::new();
    let mut current = target.to_string();

    while current != initial {
        let (parent_state, action) = parent.get(&current)?;
        reverse_steps.push((current.clone(), action.clone()));
        current = parent_state.clone();
    }

    reverse_steps.reverse();

    // Build steps: initial state (no action), then each subsequent state with its action.
    let mut steps = Vec::with_capacity(reverse_steps.len() + 1);
    steps.push(PathStep {
        state: initial.to_string(),
        action: None,
    });

    for (state, action) in &reverse_steps {
        steps.push(PathStep {
            state: state.clone(),
            action: Some(action.clone()),
        });
    }

    let length = steps.len() - 1;
    Some(ReachablePath { steps, length })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model;

    const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");

    fn build_order_model() -> TemperModel {
        model::build_model_from_ioa(ORDER_IOA, 2)
    }

    #[test]
    fn test_order_shortest_paths_to_terminal_states() {
        let m = build_order_model();
        let config = PathExtractionConfig::default();
        let result = extract_paths(&m, &config);

        // Terminal states: Cancelled, Refunded (no outgoing transitions from these).
        assert!(
            result.terminal_states.contains(&"Cancelled".to_string()),
            "Cancelled should be terminal, got: {:?}",
            result.terminal_states,
        );
        assert!(
            result.terminal_states.contains(&"Refunded".to_string()),
            "Refunded should be terminal, got: {:?}",
            result.terminal_states,
        );

        // Should have paths to both terminal states.
        assert!(
            result.paths_by_target.contains_key("Cancelled"),
            "Should have path to Cancelled",
        );
        assert!(
            result.paths_by_target.contains_key("Refunded"),
            "Should have path to Refunded",
        );
    }

    #[test]
    fn test_order_known_path_exists() {
        let m = build_order_model();
        let config = PathExtractionConfig {
            target_states: vec!["Delivered".to_string()],
            ..Default::default()
        };
        let result = extract_paths(&m, &config);

        let paths = result.paths_by_target.get("Delivered").unwrap();
        assert_eq!(paths.len(), 1);
        let path = &paths[0];

        // Draft -> Submitted -> Confirmed -> Processing -> Shipped -> Delivered
        let states: Vec<&str> = path.steps.iter().map(|s| s.state.as_str()).collect();
        assert_eq!(
            states,
            vec![
                "Draft",
                "Submitted",
                "Confirmed",
                "Processing",
                "Shipped",
                "Delivered"
            ],
        );
        assert_eq!(path.length, 5);
    }

    #[test]
    fn test_order_no_unreachable_states() {
        let m = build_order_model();
        let config = PathExtractionConfig::default();
        let result = extract_paths(&m, &config);

        assert!(
            result.unreachable_states.is_empty(),
            "All Order states should be reachable, got unreachable: {:?}",
            result.unreachable_states,
        );
    }

    #[test]
    fn test_order_states_visited() {
        let m = build_order_model();
        let config = PathExtractionConfig::default();
        let result = extract_paths(&m, &config);

        assert_eq!(
            result.states_visited,
            m.states.len(),
            "BFS should visit all states",
        );
    }

    #[test]
    fn test_max_path_length_truncates() {
        let m = build_order_model();
        let config = PathExtractionConfig {
            target_states: vec!["Delivered".to_string()],
            max_path_length: 2,
            ..Default::default()
        };
        let result = extract_paths(&m, &config);

        // Delivered requires 5 transitions from Draft, but max_path_length=2
        // means BFS won't explore past depth 2, so Delivered is unreachable.
        assert!(
            !result.paths_by_target.contains_key("Delivered"),
            "Delivered should not be reachable with max_path_length=2",
        );
    }

    #[test]
    fn test_specific_target_states_filter() {
        let m = build_order_model();
        let config = PathExtractionConfig {
            target_states: vec!["Submitted".to_string(), "Confirmed".to_string()],
            ..Default::default()
        };
        let result = extract_paths(&m, &config);

        assert!(result.paths_by_target.contains_key("Submitted"));
        assert!(result.paths_by_target.contains_key("Confirmed"));
        // Should not have paths to states not in target list.
        assert!(!result.paths_by_target.contains_key("Delivered"));
        assert!(!result.paths_by_target.contains_key("Cancelled"));
    }

    #[test]
    fn test_path_step_actions() {
        let m = build_order_model();
        let config = PathExtractionConfig {
            target_states: vec!["Submitted".to_string()],
            ..Default::default()
        };
        let result = extract_paths(&m, &config);

        let paths = result.paths_by_target.get("Submitted").unwrap();
        let path = &paths[0];

        // First step has no action (initial state).
        assert!(path.steps[0].action.is_none());
        assert_eq!(path.steps[0].state, "Draft");

        // Second step should have SubmitOrder action.
        assert_eq!(path.steps[1].action.as_deref(), Some("SubmitOrder"));
        assert_eq!(path.steps[1].state, "Submitted");
        assert_eq!(path.length, 1);
    }

    #[test]
    fn test_cancelled_path_from_draft() {
        let m = build_order_model();
        let config = PathExtractionConfig {
            target_states: vec!["Cancelled".to_string()],
            ..Default::default()
        };
        let result = extract_paths(&m, &config);

        let paths = result.paths_by_target.get("Cancelled").unwrap();
        let path = &paths[0];

        // Shortest path to Cancelled is Draft -> Cancelled (1 transition).
        assert_eq!(path.length, 1);
        assert_eq!(path.steps[0].state, "Draft");
        assert_eq!(path.steps[1].state, "Cancelled");
        assert_eq!(path.steps[1].action.as_deref(), Some("CancelOrder"));
    }
}
