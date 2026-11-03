//! A check's verdict is kept by the digest of what its levels read
//! ([`VerificationCascade::kept_key`]): a change none of them reads keeps it,
//! one they read does not, and a rule over several records is still decided
//! by the joint check, which reads every type's source.

use temper_authz::SendAnalysis;
use temper_spec::automaton::parse_automaton;
use temper_verify::VerificationCascade;
use temper_verify::composite::{Bound, Bytes, JointOutcome, Progress, verify_joint_invariants};

const NOTE: &str = include_str!("../../../test-fixtures/kept/note.ioa.toml");
const PARENT: &str = include_str!("../../../test-fixtures/kept/parent.ioa.toml");
const CHILD: &str = include_str!("../../../test-fixtures/kept/child.ioa.toml");
const ONLY_TELL_COPIES: &str = include_str!("../../../test-fixtures/kept/only_tell_copies.cedar");

/// The check `validate-ioa` runs when its caller names no settings.
fn check(ioa: &str) -> VerificationCascade {
    VerificationCascade::from_ioa(ioa)
        .with_sim_seeds(5)
        .with_prop_test_cases(100)
}

fn digest(ioa: &str) -> String {
    check(ioa).kept_key().expect("the fixture parses").digest
}

/// The one joint invariant of `parent` and `child`, under the fixture's Cedar.
fn joint(parent: &str, child: &str) -> temper_verify::composite::JointInvariantResult {
    let parent = parse_automaton(parent).expect("fixture parses");
    let child = parse_automaton(child).expect("fixture parses");
    let policies = SendAnalysis::new(ONLY_TELL_COPIES).expect("fixture policies parse");
    let (room, progress) = (Bytes::new(1 << 30), Progress::default());
    let bound = Bound {
        room: &room,
        progress: &progress,
    };
    let mut results = verify_joint_invariants(&[&parent, &child], Some(&policies), bound);
    assert_eq!(results.len(), 1);
    results.remove(0)
}

#[test]
fn a_change_no_level_reads_keeps_the_digest() {
    let reworded = NOTE.replace("Closed by its owner", "Closed by whoever owns it");
    assert_ne!(reworded, NOTE);
    assert_eq!(digest(&reworded), digest(NOTE), "a word no rule reads");
    let commented = format!("{NOTE}\n# a comment\n");
    assert_eq!(digest(&commented), digest(NOTE), "a comment");
}

#[test]
fn a_change_a_level_reads_changes_the_digest() {
    let guarded = NOTE.replace("guard = \"count < 2\"", "guard = \"count < 1\"");
    assert_ne!(guarded, NOTE);
    assert_ne!(digest(&guarded), digest(NOTE), "a guard");
    let renamed = NOTE.replace("name = \"bounded\"", "name = \"at_most_two\"");
    assert_ne!(
        digest(&renamed),
        digest(NOTE),
        "a rule's name, which a verdict reports"
    );
    let more_seeds = check(NOTE)
        .with_sim_seeds(6)
        .kept_key()
        .map(|key| key.digest);
    assert_ne!(more_seeds, Some(digest(NOTE)), "how far a level goes");
}

#[test]
fn a_rule_reading_a_related_record_keeps_the_digest_whatever_its_type_is_called() {
    let renamed = CHILD.replace("Parent[Id]", "AppTwoParent[Id]");
    assert_ne!(renamed, CHILD);
    assert_eq!(digest(&renamed), digest(CHILD), "the related type's name");

    let either = |other: &str| {
        CHILD.replace(
            "Parent[Id].status == 'Open'",
            &format!("Parent[Id].status == 'Open' || {other}[Id].status == 'Open'"),
        )
    };
    assert_ne!(
        digest(&either("Mother")),
        digest(&either("Parent")),
        "two related types are two"
    );
}

#[test]
fn a_check_that_reads_more_than_its_models_has_no_digest() {
    let scoped = check(NOTE)
        .with_composite_scope(vec![parse_automaton(NOTE).expect("fixture parses")], "Note");
    assert_eq!(scoped.kept_key(), None);
    assert_eq!(check("not a spec").kept_key(), None);
}

/// The parent's word changes from one its child's rule allows to one it
/// does not. The parent's own check reads nothing of the word, so it keeps
/// its verdict; the joint check, which reads both types' sources, refuses
/// the change.
#[test]
fn a_kept_check_does_not_keep_a_change_that_breaks_a_rule_over_two_types() {
    let stopping = PARENT.replace("value = \"'shipped'\"", "value = \"'stop'\"");
    assert_ne!(stopping, PARENT);

    assert_eq!(digest(&stopping), digest(PARENT));
    for spec in [PARENT, &stopping, CHILD] {
        let result = check(spec).run();
        assert!(
            result.all_passed,
            "each type passes its own check: {result:?}"
        );
    }

    let before = joint(PARENT, CHILD);
    assert!(before.holds(), "{before}");
    let after = joint(&stopping, CHILD);
    let JointOutcome::Broken { path } = &after.outcome else {
        panic!("expected the rule broken, got {after}");
    };
    assert!(
        path.iter()
            .filter_map(|step| step.action.as_deref())
            .any(|action| action.starts_with("Child.Copy(value='stop')")),
        "{after}"
    );
}

#[test]
fn entry_rule_changes_invalidate_a_kept_verdict() {
    let source = r#"
[automaton]
name = "Entry"
states = ["Outside", "Inside"]
initial = "Outside"
terminal = ["Inside"]
[[state]]
name = "ready"
type = "bool"
initial = true
[[action]]
name = "Enter"
from = ["Outside"]
to = "Inside"
[[entry_rule]]
name = "entry"
place = "Inside"
assert = "ready"
"#;
    let broken = source.replace("assert = \"ready\"", "assert = \"!ready\"");
    assert!(check(source).run().all_passed);
    assert!(!check(&broken).run().all_passed);
    assert_ne!(digest(source), digest(&broken));
    let related = source.replace(
        "assert = \"ready\"",
        "assert = \"Topping[id].status == 'Stocked'\"",
    );
    assert_eq!(
        digest(&related),
        digest(&related.replace("Topping[id]", "OtherTopping[id]"))
    );
}
