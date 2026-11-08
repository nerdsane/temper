//! A kind answers for every sender it stands for, asked what its split did
//! not ask, without holding each one.

use super::profile::{Kind, Profile};
use super::tests_kinds::{is, kinds_of};
use super::*;
use crate::engine::may::profile::Fact;

/// Asserts each of `senders` (an empty value is an attribute it lacks) is
/// of some kind of the split on `asked` that answers every one of
/// `actions` as the sender does, or Maybe.
fn each_has_a_kind(text: &str, asked: &[&str], actions: &[&str], senders: &[&[(&str, &str)]]) {
    let analysis = SendAnalysis::new(text).expect("the policies parse");
    let none = BTreeMap::new();
    let cause = Cause::request();
    let answers = |asker: &Asker| -> Vec<May> {
        actions
            .iter()
            .map(|action| analysis.may(asker, action, "Note", &none, Some(&cause)))
            .collect()
    };
    let kinds = kinds_of(text, asked);
    for sender in senders {
        let mut profile = Profile::default();
        for (name, value) in *sender {
            let fact = if value.is_empty() {
                Fact::Absent
            } else {
                is(value)
            };
            profile.attrs.insert((*name).to_string(), fact);
        }
        let own = answers(&Asker::OutsideAs(Kind::of(profile)));
        assert!(
            kinds.iter().any(|kind| {
                answers(&Asker::OutsideAs(kind.clone()))
                    .iter()
                    .zip(&own)
                    .all(|(kind, own)| kind == own || *kind == May::Maybe)
            }),
            "no kind answers {own:?} as {sender:?} does: {kinds:?}"
        );
    }
}

#[test]
fn a_kind_asked_what_its_split_did_not_ask_answers_for_every_sender_in_it() {
    // Asked only A, senders with no role and those with role "admin" answer
    // alike, so one kind holds both: asked Z, it may not refuse the admins.
    let text = r#"
permit(principal, action == Action::"A", resource) when { principal.role == "member" };
permit(principal, action == Action::"Z", resource) when { principal.role == "admin" };
"#;
    each_has_a_kind(
        text,
        &["A"],
        &["A", "Z"],
        &[&[("role", "admin")], &[("role", "")]],
    );
    // A child found answering as a node in another branch is held there:
    // senders with no a, whom Z refuses, may not be taken for those it lets.
    let text = r#"
forbid(principal, action == Action::"A", resource) when { principal.a == "x" };
forbid(principal, action == Action::"A", resource) when { principal.b == "x" };
permit(principal, action == Action::"A", resource) when { principal.c == "y" };
permit(principal, action == Action::"Z", resource) when { principal.a == "x" };
"#;
    each_has_a_kind(
        text,
        &["A"],
        &["A", "Z"],
        &[
            &[("a", ""), ("b", "x"), ("c", "")],
            &[("a", ""), ("b", "x"), ("c", "y")],
            &[("a", "x"), ("b", ""), ("c", "")],
            &[("a", ""), ("b", ""), ("c", "y")],
        ],
    );
}

#[test]
fn a_kind_asked_again_with_more_of_the_cause_known_answers_from_its_example() {
    // Asked Z as a reaction to some rule, senders with role a, b and c answer
    // alike, so knowing the rule or the record leaves them alike still.
    let text = r#"
permit(principal, action == Action::"A", resource)
when { principal.role == "a" || principal.role == "b" || principal.role == "c" };
permit(principal, action == Action::"Z", resource)
when { principal.role != "q" && resource.open == true };
"#;
    let analysis = SendAnalysis::new(text).expect("the policies parse");
    let named = |value: &str| Attr::Value(Json::from(value));
    let rule = Cause::reaction(
        false,
        Hop {
            rule: named("Note:A:made"),
            action: named("A"),
            entity_type: named("Note"),
            entity_id: Attr::Unknown,
        },
    );
    let questions = [
        Question {
            action: "A",
            resource_type: "Note",
            cause: Cause::request(),
        },
        Question {
            action: "Z",
            resource_type: "Note",
            cause: Cause::reaction(false, Hop::unknown()),
        },
    ];
    let open = BTreeMap::from([("open".to_string(), Attr::Value(Json::from(true)))]);
    let kinds = analysis
        .outside_kinds(&questions, &|_| Ok(()))
        .expect("fits");
    let let_in: Vec<&Answered> = kinds
        .iter()
        .filter(|found| found.answers.first() == Some(&May::Yes))
        .collect();
    assert!(!let_in.is_empty(), "no kind may send A: {kinds:?}");
    for found in let_in {
        let asker = Asker::OutsideAs(found.kind.clone());
        let may = analysis.may(&asker, "Z", "Note", &open, Some(&rule));
        assert_eq!(may, May::Yes, "{}", found.kind);
    }
}

#[test]
fn senders_found_alike_across_many_branches_are_held_in_a_few_bytes_each() {
    // Every forbid splits on two names, and only Z tells the values of one
    // apart, so most children are found answering as a node in another
    // branch. What they hold must not cost a node apiece.
    let mut text = String::from("permit(principal, action == Action::\"A\", resource);\n");
    for i in 0..10 {
        text.push_str(&format!(
            "forbid(principal, action == Action::\"A\", resource) \
             when {{ principal.a{i} == \"x\" || principal.b{i} == \"y\" }};\n\
             permit(principal, action == Action::\"Z\", resource) \
             when {{ principal.a{i} == \"w\" }};\n"
        ));
    }
    let analysis = SendAnalysis::new(&text).expect("the policies parse");
    let questions = [Question {
        action: "A",
        resource_type: "Note",
        cause: Cause::request(),
    }];
    let most = std::cell::Cell::new(0);
    analysis
        .outside_kinds(&questions, &|bytes| {
            most.set(most.get().max(bytes));
            Ok(())
        })
        .expect("fits");
    // About 9 MB here, against 374 MB when each child dropped was counted a
    // node's worth.
    assert!(most.get() < 32 << 20, "reserved {} bytes", most.get());
}

#[test]
fn senders_found_alike_keep_what_they_hold_where_they_differ() {
    // Asked Start only as a request, senders with no role and those with a
    // role but member answer alike: asked it as a reaction, the kind may
    // not let them, though it no longer knows which they are.
    let text = r#"
permit(principal, action == Action::"Add", resource is Ping);
permit(principal, action in [Action::"Start", Action::"Finish"], resource is Job)
when { principal has role && principal.role == "member" };
"#;
    let analysis = SendAnalysis::new(text).expect("the policies parse");
    let questions = [
        Question {
            action: "Add",
            resource_type: "Ping",
            cause: Cause::request(),
        },
        Question {
            action: "Start",
            resource_type: "Job",
            cause: Cause::request(),
        },
    ];
    let reaction = Cause::reaction(false, Hop::unknown());
    let none = BTreeMap::new();
    let kinds = analysis
        .outside_kinds(&questions, &|_| Ok(()))
        .expect("fits");
    let refused: Vec<&Answered> = kinds
        .iter()
        .filter(|found| found.answers[1] == May::No)
        .collect();
    assert!(!refused.is_empty(), "every kind may Start: {kinds:?}");
    for found in refused {
        let asker = Asker::OutsideAs(found.kind.clone());
        let may = analysis.may(&asker, "Start", "Job", &none, Some(&reaction));
        assert_eq!(may, May::No, "{:?}", found.kind);
    }
}
