//! Every sender is of a kind that answers as it does.

use super::profile::{Fact, Kind, Profile};
use super::*;

/// Policies shaped like an app's: an owner, members, guests by role and
/// pattern, kernel-only context, and forbids that read the same tests.
const POLICIES: &str = r#"
permit(principal, action in [Action::"Add", Action::"Look"], resource is Note)
when { ((principal is Customer && principal.id == "owner") ||
        (context has actingFor && context.actingFor == "owner") ||
        (principal has Member && principal.Member == "Active") ||
        (principal has actingForMember && principal.actingForMember == "Active")) ||
       (principal has role && ((principal is Customer && (principal.role == "guest:link" || principal.role == "guest:everyone")) ||
        (principal is Agent && (principal.role like "guest:link:*" || principal.role like "guest:everyone:*")))) };
permit(principal, action == Action::"Done", resource is Note)
when { context has via && context.via != "request" };
permit(principal, action == Action::"Done", resource is Note)
when { !(context has via) && ((principal is Customer && principal.id == "owner") ||
       (principal has Member && principal.Member == "Active")) };
forbid(principal is Agent, action, resource is Note)
when { context has actingFor && !(principal has role && (principal.role == "helper" || principal.role like "guest:*:helper" || principal.role == "service" || principal.role like "*-*")) }
unless { action == Action::"Look" || (context has via && context.via != "request") };
forbid(principal, action, resource is Note)
when { principal has role && principal.role like "guest*" }
unless { action == Action::"Look" || (context has via && context.via != "request") };
permit(principal, action == Action::"Hook", resource is Note)
when { principal.id == "hook-one" || (principal has agent_type && principal.agent_type == "hook") };
permit(principal, action == Action::"Other", resource is Elsewhere)
when { principal.id == "hook-two" };
"#;

pub(super) fn is(value: &str) -> Fact {
    Fact::Is(Json::from(value))
}

/// Every sender of this small world: each value any policy compares with,
/// values each pattern matches, and values none does.
fn senders() -> Vec<Profile> {
    let optional = |values: &[&str]| -> Vec<Fact> {
        std::iter::once(Fact::Absent)
            .chain(values.iter().map(|value| is(value)))
            .collect()
    };
    let roles = optional(&[
        "guest:link",
        "guest:everyone",
        "guest:link:x",
        "guest:everyone:x",
        "guest:a:helper",
        "guest",
        "helper",
        "service",
        "a-b",
        "other",
    ]);
    let members = optional(&["Active", "Gone"]);
    let types = optional(&["hook", "other"]);
    let acting = optional(&["owner", "else"]);
    let ids = [is("owner"), is("hook-one"), is("hook-two"), is("else")];
    let mut all = Vec::new();
    for kind in ["Customer", "Agent", "Service"] {
        for role in &roles {
            for member in &members {
                for acting_member in &members {
                    for agent_type in &types {
                        for id in &ids {
                            for acting_for in &acting {
                                let mut profile = Profile {
                                    kind: Some(is(kind)),
                                    ..Profile::default()
                                };
                                let attrs = [
                                    ("role", role),
                                    ("Member", member),
                                    ("actingForMember", acting_member),
                                    ("agent_type", agent_type),
                                    ("id", id),
                                ];
                                for (name, fact) in attrs {
                                    profile.attrs.insert(name.to_string(), fact.clone());
                                }
                                profile
                                    .context
                                    .insert("actingFor".to_string(), acting_for.clone());
                                all.push(profile);
                            }
                        }
                    }
                }
            }
        }
    }
    all
}

#[test]
fn every_sender_is_of_a_kind_that_answers_as_it_does() {
    let analysis = SendAnalysis::new(POLICIES).expect("the policies parse");
    let causes = [Cause::request(), Cause::reaction(false, Hop::unknown())];
    let actions = ["Add", "Look", "Done", "Hook"];
    let questions: Vec<Question<'_>> = causes
        .iter()
        .flat_map(|cause| {
            actions.iter().map(|action| Question {
                action,
                resource_type: "Note",
                cause: cause.clone(),
            })
        })
        .collect();
    let answers = |asker: &Asker| -> Vec<May> {
        let none = BTreeMap::new();
        questions
            .iter()
            .map(|q| analysis.may(asker, q.action, q.resource_type, &none, Some(&q.cause)))
            .collect()
    };
    let kinds: Vec<Vec<May>> = analysis
        .outside_kinds(&questions, &|_| Ok(()))
        .expect("nothing limits the room")
        .into_iter()
        .map(|found| {
            let own = answers(&Asker::OutsideAs(found.kind));
            assert_eq!(
                own, found.answers,
                "the partition's answers are the kind's own"
            );
            own
        })
        .collect();
    assert!(!kinds.is_empty());
    let mut seen = 0;
    for sender in senders() {
        let own = answers(&Asker::OutsideAs(Kind::of(sender.clone())));
        seen += 1;
        // Exactly: a kind that answers Maybe to everything would cover every
        // sender and tell the walk nothing.
        assert!(
            kinds.contains(&own),
            "no kind answers as {sender} does: {own:?}"
        );
    }
    assert!(seen > 10_000, "the world holds {seen} senders");
}

#[test]
fn senders_who_read_different_tests_are_not_taken_for_each_other() {
    // Agents may Look with one role and customers with another, and both may
    // Add with the second: the same effects, on different roles.
    let analysis = SendAnalysis::new(
        r#"
permit(principal is Agent, action == Action::"Look", resource is Note)
when { principal has role && principal.role == "a" };
permit(principal is Customer, action == Action::"Look", resource is Note)
when { principal has role && principal.role == "b" };
permit(principal, action == Action::"Add", resource is Note)
when { principal has role && principal.role == "b" };
"#,
    )
    .expect("the policies parse");
    let cause = Cause::request();
    let questions = ["Look", "Add"].map(|action| Question {
        action,
        resource_type: "Note",
        cause: cause.clone(),
    });
    let answers = |asker: &Asker| -> Vec<May> {
        let none = BTreeMap::new();
        questions
            .iter()
            .map(|q| analysis.may(asker, q.action, q.resource_type, &none, Some(&q.cause)))
            .collect()
    };
    let kinds: Vec<Vec<May>> = analysis
        .outside_kinds(&questions, &|_| Ok(()))
        .expect("nothing limits the room")
        .into_iter()
        .map(|found| {
            let own = answers(&Asker::OutsideAs(found.kind));
            assert_eq!(
                own, found.answers,
                "the partition's answers are the kind's own"
            );
            own
        })
        .collect();
    for (kind, role) in [("Agent", "a"), ("Agent", "b"), ("Customer", "b")] {
        let mut sender = Profile {
            kind: Some(is(kind)),
            ..Profile::default()
        };
        sender.attrs.insert("role".to_string(), is(role));
        let own = answers(&Asker::OutsideAs(Kind::of(sender)));
        assert!(
            kinds.contains(&own),
            "no kind answers as {kind} {role}: {own:?}"
        );
    }
}

#[test]
fn a_list_of_people_tells_senders_apart_as_comparing_with_each_does() {
    // Whoever a list lets start a move is who the same list lets finish it: a
    // kind that joined them would be allowed the one and refused the other.
    let policies = SendAnalysis::new(
        r#"permit(principal, action == Action::"Ban", resource)
            when { principal is Customer && ["a", "b"].contains(principal.id) };
           permit(principal, action == Action::"Stop", resource)
            when { principal is Customer && ["a", "b"].contains(principal.id) };"#,
    )
    .expect("the test policy parses");
    let cause = Cause::reaction(false, Hop::unknown());
    let question = |action| Question {
        action,
        resource_type: "Feed",
        cause: cause.clone(),
    };
    let kinds = policies
        .outside_kinds(&[question("Ban"), question("Stop")], &|_| Ok(()))
        .expect("the test policy parses")
        .into_iter()
        .map(|found| found.kind)
        .collect::<Vec<Kind>>();
    let none = BTreeMap::new();
    let may = |kind: &Kind, action| {
        let asker = Asker::OutsideAs(kind.clone());
        policies.may(&asker, action, "Feed", &none, Some(&cause))
    };
    for kind in &kinds {
        assert_ne!(may(kind, "Ban"), May::Maybe, "{kind}");
        assert_eq!(may(kind, "Ban"), may(kind, "Stop"), "{kind}");
    }
    assert!(kinds.iter().any(|kind| may(kind, "Ban") == May::Yes));
    assert!(kinds.iter().any(|kind| may(kind, "Ban") == May::No));
}

/// The senders each kind describes, whatever its split asked.
pub(super) fn described(kinds: &[Kind]) -> Vec<(Vec<Profile>, Vec<Profile>)> {
    kinds
        .iter()
        .map(|kind| (kind.examples.clone(), kind.others.clone()))
        .collect()
}

pub(super) fn kinds_of(policies: &str, actions: &[&str]) -> Vec<Kind> {
    let analysis = SendAnalysis::new(policies).expect("policies parse");
    let questions: Vec<Question<'_>> = actions
        .iter()
        .map(|action| Question {
            action,
            resource_type: "Note",
            cause: Cause::request(),
        })
        .collect();
    analysis
        .outside_kinds(&questions, &|_| Ok(()))
        .expect("fits")
        .into_iter()
        .map(|found| found.kind)
        .collect::<Vec<Kind>>()
}

#[test]
fn a_policy_the_actions_leave_alike_is_read_once_and_tells_the_same_kinds() {
    // The forbid reads the action, but leaves Add and Edit alike: members,
    // and the rest, as Add alone says.
    let policies = r#"
permit(principal, action, resource is Note)
when { principal has role && principal.role == "member" };
forbid(principal, action, resource is Note)
unless { action in [Action::"Add", Action::"Edit"] || (context has via && context.via != "request") };
"#;
    let alone = kinds_of(policies, &["Add"]);
    assert_eq!(alone.len(), 2, "members, and the rest");
    assert_eq!(
        described(&kinds_of(policies, &["Add", "Edit"])),
        described(&alone)
    );
}

#[test]
fn a_policy_two_actions_leave_differently_still_tells_their_senders_apart() {
    // Members may Add and helpers may Edit: the same condition text, left
    // open for one action and decided for the other.
    let policies = r#"
permit(principal, action, resource is Note)
when { principal has role && ((principal.role == "member" && action == Action::"Add") || (principal.role == "helper" && action == Action::"Edit")) };
"#;
    assert_eq!(
        kinds_of(policies, &["Add", "Edit"]).len(),
        3,
        "members, helpers, and the rest"
    );
}

#[test]
fn each_kind_carries_its_own_answer_to_every_question() {
    // Members may Add and Edit alike, helpers may Look, everyone may Open,
    // and nobody may Drop: answers asked of a leaf, answers one question
    // stands in for, and answers someone unknown already has.
    let analysis = SendAnalysis::new(
        r#"
permit(principal, action in [Action::"Add", Action::"Edit"], resource is Note)
when { principal has role && principal.role == "member" };
permit(principal, action == Action::"Look", resource is Note)
when { principal has role && principal.role == "helper" };
permit(principal, action == Action::"Open", resource is Note);
"#,
    )
    .expect("the policies parse");
    let questions = ["Drop", "Add", "Look", "Open", "Edit"].map(|action| Question {
        action,
        resource_type: "Note",
        cause: Cause::request(),
    });
    let none = BTreeMap::new();
    let found = analysis
        .outside_kinds(&questions, &|_| Ok(()))
        .expect("nothing limits the room");
    let mut seen = Vec::new();
    for Answered { kind, answers } in found {
        let asker = Asker::OutsideAs(kind.clone());
        let own: Vec<May> = questions
            .iter()
            .map(|q| analysis.may(&asker, q.action, q.resource_type, &none, Some(&q.cause)))
            .collect();
        assert_eq!(answers, own, "{kind}");
        seen.push(answers);
    }
    seen.sort_by_key(|answers| format!("{answers:?}"));
    seen.dedup();
    assert_eq!(seen.len(), 3, "members, helpers, and the rest: {seen:?}");
}

#[test]
fn a_value_only_an_unasked_policy_names_leaves_no_sender_without_a_kind() {
    // Z names context.role "member", but only A is asked: a sender whose
    // role and context.role are both "member" may still send A.
    let asked = r#"
permit(principal, action == Action::"A", resource)
when { principal.role == context.role && principal.role == "member" };
"#;
    let with_unasked = format!(
        r#"{asked}
permit(principal, action == Action::"Z", resource) when {{ context.role == "member" }};
"#
    );
    let mut sender = Profile::default();
    sender.attrs.insert("role".to_string(), is("member"));
    sender.context.insert("role".to_string(), is("member"));
    let analysis = SendAnalysis::new(&with_unasked).expect("the policies parse");
    let none = BTreeMap::new();
    let cause = Cause::request();
    let own = analysis.may(
        &Asker::OutsideAs(Kind::of(sender)),
        "A",
        "Note",
        &none,
        Some(&cause),
    );
    assert_eq!(own, May::Yes);
    let kinds = kinds_of(&with_unasked, &["A"]);
    assert!(
        kinds.iter().any(|kind| {
            let asker = Asker::OutsideAs(kind.clone());
            analysis.may(&asker, "A", "Note", &none, Some(&cause)) != May::No
        }),
        "every kind refuses A: {kinds:?}"
    );
    assert_eq!(kinds, kinds_of(asked, &["A"]));
}
