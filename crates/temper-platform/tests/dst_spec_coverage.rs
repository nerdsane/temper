//! Drift-proof direct DST coverage for platform and maintained reference specs.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use temper_server::entity_actor::dst_coverage::explore_ioa_spec;

const SCENARIO_BUDGET_PER_SPEC: usize = 1_024;
const MIN_GENERATED_SCENARIOS: usize = 1_000;

fn ioa_paths(directory: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<_> =
        std::fs::read_dir(directory) // determinism-ok: sorted test-only spec discovery
            .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()))
            .map(|entry| entry.expect("spec directory entry").path())
            .filter(|path| path.to_string_lossy().ends_with(".ioa.toml"))
            .collect();
    paths.sort();
    paths
}

#[test]
fn every_maintained_entity_state_action_and_invariant_has_direct_dst_coverage() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest
        .parent()
        .and_then(Path::parent)
        .expect("workspace root");
    let directories = [
        manifest.join("src/specs"),
        workspace.join("reference-apps/ecommerce/specs"),
        workspace.join("reference-apps/oncall/specs"),
    ];

    let mut paths = Vec::new();
    for directory in &directories {
        paths.extend(ioa_paths(directory));
    }
    assert_eq!(
        paths.len(),
        28,
        "maintained spec inventory changed; inspect coverage"
    );

    let mut generated_scenarios = 0;
    for path in paths {
        let source =
            std::fs::read_to_string(&path) // determinism-ok: test-only immutable spec input
                .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        let automaton = temper_spec::automaton::parse_automaton(&source)
            .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()));
        let coverage = explore_ioa_spec(&source, SCENARIO_BUDGET_PER_SPEC);
        generated_scenarios += coverage.generated_scenarios;

        let declared_states: BTreeSet<_> = automaton.automaton.states.iter().cloned().collect();
        let declared_actions: BTreeSet<_> = automaton
            .actions
            .iter()
            .filter(|action| action.kind != "output")
            .map(|action| action.name.clone())
            .collect();
        let declared_invariants: BTreeSet<_> = automaton
            .invariants
            .iter()
            .map(|invariant| invariant.name.clone())
            .collect();

        assert_eq!(
            coverage.entity_type,
            automaton.automaton.name,
            "{} entity mismatch",
            path.display()
        );
        assert_eq!(
            coverage.states,
            declared_states,
            "{} has states without direct simulation coverage",
            path.display()
        );
        assert_eq!(
            coverage.actions,
            declared_actions,
            "{} has actions without direct simulation coverage",
            path.display()
        );
        assert_eq!(
            coverage.invariants,
            declared_invariants,
            "{} has invariants without runtime evaluators",
            path.display()
        );
    }

    assert!(
        generated_scenarios >= MIN_GENERATED_SCENARIOS,
        "generated/property DST workload must run at least {MIN_GENERATED_SCENARIOS} scenarios"
    );
}
