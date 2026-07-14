//! Capability regression over every checked-in production IOA invariant.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use temper_verify::{InvariantKind, VerificationCascade, build_model_from_ioa};

#[test]
fn every_production_invariant_is_typed_or_rejected_with_its_exact_span() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    for root in ["crates", "os-apps", "reference-apps"] {
        collect_ioa_files(&repo.join(root), &mut files);
    }
    files.sort();

    let mut declarations = 0usize;
    let mut supported = 0usize;
    let mut unsupported = 0usize;
    let mut forms = BTreeMap::<String, (usize, bool)>::new();

    for path in &files {
        let source = std::fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        let model = build_model_from_ioa(&source, 2)
            .unwrap_or_else(|error| panic!("build {}: {error}", path.display()));
        let declared = &model.invariants[1..];
        declarations += declared.len();

        let unsupported_here: Vec<_> = declared
            .iter()
            .filter_map(|invariant| {
                let InvariantKind::Unverifiable { expression } = &invariant.kind else {
                    supported += 1;
                    return None;
                };
                unsupported += 1;
                Some((invariant.name.as_str(), expression.as_str()))
            })
            .collect();

        for invariant in declared {
            let (expression, is_unsupported) = match &invariant.kind {
                InvariantKind::Unverifiable { expression } => (expression.clone(), true),
                _ => {
                    let span = invariant
                        .source_span
                        .expect("supported invariant source span");
                    (source[span.start.byte..span.end.byte].to_string(), false)
                }
            };
            let entry = forms.entry(expression).or_insert((0, is_unsupported));
            entry.0 += 1;
            assert_eq!(
                entry.1, is_unsupported,
                "the same assertion form cannot be both supported and unsupported"
            );
        }

        if unsupported_here.is_empty() {
            continue;
        }

        let result = VerificationCascade::from_ioa(&source).run();
        assert!(
            !result.all_passed,
            "{} passed unsupported claims",
            path.display()
        );
        assert!(
            result.levels.is_empty(),
            "{} ran a backend after capability failure",
            path.display()
        );
        assert_eq!(
            result.errors.len(),
            unsupported_here.len(),
            "{}",
            path.display()
        );
        for (error, (name, expression)) in result.errors.iter().zip(unsupported_here) {
            assert_eq!(error.code, "TVE001");
            assert_eq!(error.invariant, name);
            assert_eq!(error.assertion, expression);
            assert_eq!(
                &source[error.source_span.start.byte..error.source_span.end.byte],
                expression,
                "{} returned an inexact source range",
                path.display()
            );
        }
    }

    assert_eq!(files.len(), 110, "production IOA corpus changed");
    assert_eq!(declarations, 120, "invariant declaration corpus changed");
    assert_eq!(supported, 120, "supported invariant corpus changed");
    assert_eq!(unsupported, 0, "rejected invariant corpus changed");
    assert_eq!(forms.len(), 53, "distinct assertion-form corpus changed");

    let rejected: BTreeMap<_, _> = forms
        .iter()
        .filter(|(_, (_, is_rejected))| *is_rejected)
        .map(|(form, (count, _))| (form.as_str(), *count))
        .collect();
    let expected_rejected = BTreeMap::new();
    assert_eq!(
        rejected, expected_rejected,
        "rejected assertion forms changed"
    );
    eprintln!(
        "production invariant corpus: {} specs, {declarations} declarations, {supported} supported, {unsupported} rejected, {} distinct forms",
        files.len(),
        forms.len()
    );
    for (form, (count, rejected)) in forms {
        eprintln!(
            "{count:>3} {} {form}",
            if rejected { "REJECT" } else { "TYPED " }
        );
    }
}

fn collect_ioa_files(directory: &Path, files: &mut Vec<PathBuf>) {
    if !directory.exists() {
        return;
    }
    let entries = std::fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("read directory {}: {error}", directory.display()));
    for entry in entries {
        let entry = entry.expect("read directory entry");
        let path = entry.path();
        if path.is_dir() {
            collect_ioa_files(&path, files);
        } else if path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().ends_with(".ioa.toml"))
        {
            files.push(path);
        }
    }
}
