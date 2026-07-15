use std::fs;
use std::path::{Path, PathBuf};

use temper_spec::automaton::{LivenessEnforcement, parse_automaton_with_liveness};

const SPEC_ROOTS: [&str; 6] = [
    "crates/temper-agents/specs",
    "crates/temper-platform/src/specs",
    "docs/examples/pipeline-specs",
    "os-apps",
    "reference-apps",
    "test-fixtures/specs",
];

#[test]
fn every_tracked_ioa_spec_parses_and_round_trips_through_the_canonical_schema() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("temper-spec must live under <workspace>/crates");
    let mut paths = Vec::new();
    for root in SPEC_ROOTS {
        collect_ioa_specs(&workspace.join(root), &mut paths);
    }
    paths.sort();
    paths.dedup();
    assert!(
        paths.len() >= 130,
        "expected the full repository IOA corpus"
    );

    let mut failures = Vec::new();
    for path in paths {
        let source = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
        let relative = path.strip_prefix(workspace).unwrap_or(&path);
        let parsed = match parse_automaton_with_liveness(&source, LivenessEnforcement::WarnOnly) {
            Ok(parsed) => parsed,
            Err(error) => {
                failures.push(format!("{}: initial parse: {error}", relative.display()));
                continue;
            }
        };
        let canonical = match toml::to_string(&parsed) {
            Ok(canonical) => canonical,
            Err(error) => {
                failures.push(format!(
                    "{}: canonical serialization: {error}",
                    relative.display()
                ));
                continue;
            }
        };
        let reparsed =
            match parse_automaton_with_liveness(&canonical, LivenessEnforcement::WarnOnly) {
                Ok(reparsed) => reparsed,
                Err(error) => {
                    failures.push(format!(
                        "{}: canonical round-trip parse: {error}",
                        relative.display()
                    ));
                    continue;
                }
            };
        match toml::to_string(&reparsed) {
            Ok(reserialized) if reserialized == canonical => {}
            Ok(_) => failures.push(format!(
                "{}: canonical serialization changed after round trip",
                relative.display()
            )),
            Err(error) => failures.push(format!(
                "{}: canonical round-trip serialization: {error}",
                relative.display()
            )),
        }
    }

    assert!(
        failures.is_empty(),
        "repository IOA corpus failures:\n{}",
        failures.join("\n")
    );
}

fn collect_ioa_specs(root: &Path, paths: &mut Vec<PathBuf>) {
    // Missing roots are skipped so optional app trees do not panic the corpus.
    let Ok(read_dir) = fs::read_dir(root) else {
        return;
    };
    let mut entries = read_dir
        .map(|entry| entry.expect("directory entry must be readable"))
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        let path = entry.path();
        let file_type = entry
            .file_type()
            .unwrap_or_else(|error| panic!("failed to stat {}: {error}", path.display()));
        if file_type.is_dir() {
            collect_ioa_specs(&path, paths);
        } else if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".ioa.toml"))
        {
            paths.push(path);
        }
    }
}
