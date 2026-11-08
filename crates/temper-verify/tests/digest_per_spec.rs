//! A joint proof's digest is the same however many times its specs and Cedar
//! are read. A kept verdict is found again by that digest, so a digest that
//! changes from one reading to the next is never found: every request walks
//! the proof again, and a walk longer than one request never ends.
//!
//! Each reading builds every value anew, so a hash table whose order is
//! seeded per table (`HashMap`, `HashSet`) and reaches the digest's text
//! prints its entries in a new order and fails this.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use temper_authz::SendAnalysis;
use temper_spec::automaton::parse_automaton;
use temper_verify::composite::{Bytes, joint_proofs};

/// How many times each app is read.
const READINGS: usize = 5;

/// An app: its specs, and the Cedar that decides who may send what.
struct App {
    name: String,
    specs: Vec<String>,
    cedar: String,
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn files(dir: &Path, suffix: &str) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|entry| entry.expect("a directory entry").path())
        .filter(|path| path.to_string_lossy().ends_with(suffix))
        .collect();
    found.sort();
    found
}

/// The specs in `dir` with the Cedar in `dir` and in its `policies/`.
fn app_in(dir: &Path) -> App {
    let policies = dir.join("policies");
    let mut cedar = files(dir, ".cedar");
    if policies.is_dir() {
        cedar.extend(files(&policies, ".cedar"));
    }
    App {
        name: dir.display().to_string(),
        specs: files(dir, ".ioa.toml").iter().map(|p| read(p)).collect(),
        cedar: cedar.iter().map(|p| read(p)).collect::<Vec<_>>().join("\n"),
    }
}

/// Every spec fixture with Cedar: each directory whose specs make one app,
/// and in a directory of separate models, each Cedar file with the specs
/// named after it.
fn apps() -> Vec<App> {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixtures = crate_dir.join("../../test-fixtures");
    let race_models = crate_dir.join("tests/race_models");
    let mut apps: Vec<App> = fs::read_dir(&race_models)
        .expect("race models")
        .map(|entry| entry.expect("a directory entry").path())
        .filter(|path| path.is_dir())
        .map(|dir| app_in(&dir))
        .collect();
    apps.push(app_in(&fixtures.join("kept")));
    apps.push(app_in(&fixtures.join("specs")));

    let separate = crate_dir.join("tests/fair_models");
    for cedar in files(&separate, ".cedar") {
        let stem = cedar
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".cedar"))
            .expect("a Cedar file name")
            .to_string();
        let specs = files(&separate, ".ioa.toml")
            .into_iter()
            .filter(|spec| {
                let name = spec.file_name().and_then(|n| n.to_str()).unwrap_or("");
                name == format!("{stem}.ioa.toml") || name.starts_with(&format!("{stem}."))
            })
            .map(|p| read(&p))
            .collect();
        apps.push(App {
            name: cedar.display().to_string(),
            specs,
            cedar: read(&cedar),
        });
    }
    apps.sort_by(|a, b| a.name.cmp(&b.name));
    apps
}

/// One reading of `app`: each joint proof's name and digest, built from the
/// text again. A spec that does not parse is left out of every reading alike.
fn digests(app: &App) -> BTreeMap<String, String> {
    let automatons: Vec<_> = app
        .specs
        .iter()
        .filter_map(|spec| parse_automaton(spec).ok())
        .collect();
    let refs: Vec<_> = automatons.iter().collect();
    let policies =
        SendAnalysis::new(&app.cedar).unwrap_or_else(|e| panic!("{}: Cedar parses: {e}", app.name));
    let room = Bytes::new(1 << 30);
    let mut found = BTreeMap::new();
    for proof in joint_proofs(&refs, Some(&policies), &room) {
        let earlier = found.insert(proof.name(), proof.digest());
        assert!(
            earlier.is_none(),
            "{}: two proofs named {}",
            app.name,
            proof.name()
        );
    }
    found
}

#[test]
fn every_joint_proof_has_one_digest_however_often_its_app_is_read() {
    let apps = apps();
    assert!(apps.len() >= 10, "the fixtures were found: {}", apps.len());
    let mut proofs = 0;
    for app in &apps {
        let first = digests(app);
        proofs += first.len();
        for reading in 1..READINGS {
            let again = digests(app);
            assert_eq!(
                again, first,
                "{}: reading {reading} gave other digests than the first",
                app.name
            );
        }
    }
    assert!(proofs >= 10, "the fixtures hold joint proofs: {proofs}");
}
