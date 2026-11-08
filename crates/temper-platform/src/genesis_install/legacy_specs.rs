//! Upgrade migration for Genesis app bundles restored at startup.
//!
//! An app installed from Genesis by an earlier kernel is materialized again on
//! every boot from its pinned ref, and that ref may hold specs in the old
//! predicate syntax, which this kernel refuses. Converting the restored copy
//! keeps the installed app working until it is reinstalled from a converted
//! ref. New installs are not converted: they must already be current.

use std::path::{Path, PathBuf};

/// Upper bound on spec files examined under one cache root.
const MAX_SPEC_FILES: usize = 4096;
/// Upper bound on directory depth walked under one cache root.
const MAX_DEPTH: usize = 8;

/// Rewrite every `*.ioa.toml` under `root` that is in the old syntax and
/// converts cleanly. Returns how many files were rewritten. A file that
/// neither parses nor converts is left as it is; installing it fails the way
/// it would have without this step.
pub(super) fn convert_legacy_bundle_specs(root: &Path) -> usize {
    let mut files = Vec::new();
    collect_spec_files(root, 0, &mut files);
    debug_assert!(files.len() <= MAX_SPEC_FILES);

    let mut converted = 0usize;
    for path in files {
        let Ok(source) = std::fs::read_to_string(&path) else {
            continue;
        };
        if temper_spec::automaton::parse_automaton(&source).is_ok() {
            continue;
        }
        match temper_spec::automaton::legacy::migrate_source(&source) {
            Ok(migration) => match std::fs::write(&path, &migration.source) {
                Ok(()) => {
                    converted += 1;
                    tracing::info!(
                        path = %path.display(),
                        notes = migration.notes.len(),
                        "converted restored Genesis spec to the current syntax"
                    );
                }
                Err(error) => tracing::warn!(
                    path = %path.display(),
                    %error,
                    "failed to write converted Genesis spec"
                ),
            },
            Err(error) => tracing::warn!(
                path = %path.display(),
                %error,
                "restored Genesis spec does not parse and cannot be converted"
            ),
        }
    }
    converted
}

fn collect_spec_files(dir: &Path, depth: usize, files: &mut Vec<PathBuf>) {
    if depth > MAX_DEPTH || files.len() >= MAX_SPEC_FILES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if files.len() >= MAX_SPEC_FILES {
            return;
        }
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            collect_spec_files(&path, depth + 1, files);
        } else if file_type.is_file()
            && path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with(".ioa.toml"))
        {
            files.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEGACY: &str = r#"
[automaton]
name = "Task"
states = ["Open", "Done"]
initial = "Open"

[[state]]
name = "ready"
type = "bool"
initial = "false"

[[action]]
name = "Finish"
from = ["Open"]
to = "Done"
guard = [{ type = "is_true", var = "ready" }]
"#;

    const UNREADABLE: &str = "[automaton]\nname = \"Broken\"\nstates = [\n";

    #[test]
    fn converts_old_syntax_specs_and_leaves_the_rest() {
        let root = tempfile::tempdir().expect("tempdir");
        let specs = root.path().join("some-app").join("specs");
        std::fs::create_dir_all(&specs).expect("create specs dir");
        let current = temper_spec::automaton::legacy::migrate_source(LEGACY)
            .expect("fixture converts")
            .source;
        std::fs::write(specs.join("task.ioa.toml"), LEGACY).expect("write legacy");
        std::fs::write(specs.join("current.ioa.toml"), &current).expect("write current");
        std::fs::write(specs.join("broken.ioa.toml"), UNREADABLE).expect("write broken");
        std::fs::write(specs.join("notes.toml"), LEGACY).expect("write non-spec");

        assert_eq!(convert_legacy_bundle_specs(root.path()), 1);

        let task = std::fs::read_to_string(specs.join("task.ioa.toml")).expect("read task");
        assert!(temper_spec::automaton::parse_automaton(&task).is_ok());
        assert_eq!(
            std::fs::read_to_string(specs.join("current.ioa.toml")).expect("read current"),
            current,
            "a current spec is not rewritten"
        );
        assert_eq!(
            std::fs::read_to_string(specs.join("broken.ioa.toml")).expect("read broken"),
            UNREADABLE
        );
        assert_eq!(
            std::fs::read_to_string(specs.join("notes.toml")).expect("read notes"),
            LEGACY,
            "only *.ioa.toml files are specs"
        );
        assert_eq!(
            convert_legacy_bundle_specs(root.path()),
            0,
            "a second pass changes nothing"
        );
    }
}
