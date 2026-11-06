use super::*;
use crate::{AuthzEngine, SecurityContext};
use std::collections::HashMap;

#[test]
fn cedar_set_equality_denies_the_same_reaction_as_the_real_engine() {
    for (expression, denied) in [
        ("[1, 2] == [2, 1]", true),
        ("[1, 2, 1] == [2, 1]", true),
        ("[[1, 2], [3]] == [[3, 3], [2, 1, 1]]", true),
        ("[1, 2] != [2, 1]", false),
        ("[1, 2] == [1, 3]", false),
        ("[] == []", true),
    ] {
        let policies = format!(
            r#"permit(principal, action, resource);
            forbid(principal, action == Action::"Bring", resource is Place)
            when {{ {expression} }};"#
        );
        let analysis = SendAnalysis::new(&policies).expect("policies parse");
        let asker = Asker::Service {
            name: "place".into(),
            context: BTreeMap::new(),
        };
        let engine = AuthzEngine::new(&policies).expect("policies parse");
        let context = SecurityContext::from_resolved_identity("service:place", "place", None);
        let decision = engine.authorize(&context, "Bring", "Place", &HashMap::new());
        assert_eq!(decision.is_allowed(), !denied, "{expression}: {decision:?}");
        assert_eq!(
            analysis.may(&asker, "Bring", "Place", &BTreeMap::new(), None),
            if denied { May::No } else { May::Yes },
            "{expression}"
        );
    }
}

#[test]
fn a_known_cause_is_read_as_the_engine_reads_it() {
    use crate::{MoveCause, ReactionHop, TimerArm};
    let hop = || ReactionHop {
        rule: "Place:Ask:bring".into(),
        action: "Ask".into(),
        entity_type: "Place".into(),
        entity_id: "p1".into(),
    };
    let timer = MoveCause::timer_fire(TimerArm {
        armed_by: "someone".into(),
        state_timer_of: None,
    });
    let causes = [
        (None, Cause::request()),
        (
            Some(MoveCause::reaction(hop(), None)),
            Cause::reaction(false, Hop::unknown()),
        ),
        (
            Some(MoveCause::reaction(hop(), Some(&timer))),
            Cause::reaction(true, Hop::unknown()),
        ),
    ];
    for when in [
        r#"context.via != "request""#,
        r#"context has via && context.via == "reaction""#,
        r#"context.via == "timer" && context has armedBy"#,
        r#"context has rule"#,
    ] {
        let policies =
            format!(r#"permit(principal, action == Action::"Bring", resource) when {{ {when} }};"#);
        let analysis = SendAnalysis::new(&policies).expect("policies parse");
        let engine = AuthzEngine::new(&policies).expect("policies parse");
        for (move_cause, cause) in &causes {
            let mut context = SecurityContext::from_resolved_identity("someone", "agent", None);
            context.cause = move_cause.clone();
            let allowed = engine
                .authorize(&context, "Bring", "Place", &HashMap::new())
                .is_allowed();
            let may = analysis.may(
                &Asker::Outside,
                "Bring",
                "Place",
                &BTreeMap::new(),
                Some(cause),
            );
            assert_eq!(
                may,
                if allowed { May::Yes } else { May::No },
                "{when}, {cause:?}"
            );
        }
    }
}

#[test]
fn every_outside_principal_is_of_one_kind_that_answers_as_the_engine_does() {
    use crate::{MoveCause, ReactionHop};
    let policies = r#"
permit(principal, action == Action::"Add", resource is Note)
when { principal has role && (principal.role == "member" || principal.role like "guest*") };
permit(principal, action == Action::"Address", resource is Note)
when { context has via && context.via != "request" };
forbid(principal, action == Action::"Address", resource is Note)
when { principal has role && principal.role like "guest*" };
"#;
    let analysis = SendAnalysis::new(policies).expect("policies parse");
    let engine = AuthzEngine::new(policies).expect("policies parse");
    let questions = [
        Question {
            action: "Add",
            resource_type: "Note",
            cause: Cause::request(),
        },
        Question {
            action: "Address",
            resource_type: "Note",
            cause: Cause::reaction(false, Hop::unknown()),
        },
    ];
    let kinds = analysis.outside_kinds(&questions, &|_| Ok(())).unwrap();
    let answers = |kind: &Kind| -> Vec<May> {
        let asker = Asker::OutsideAs(kind.clone());
        questions
            .iter()
            .map(|q| {
                analysis.may(
                    &asker,
                    q.action,
                    q.resource_type,
                    &BTreeMap::new(),
                    Some(&q.cause),
                )
            })
            .collect()
    };
    // The policies decide both moves for each kind.
    for kind in &kinds {
        assert!(
            !answers(kind).contains(&May::Maybe),
            "{kind}: {:?}",
            answers(kind)
        );
    }
    let hop = ReactionHop {
        rule: "Note:Add:address".into(),
        action: "Add".into(),
        entity_type: "Note".into(),
        entity_id: "n1".into(),
    };
    for role in [
        None,
        Some("member"),
        Some("guest:link"),
        Some("guestbook"),
        Some("visitor"),
    ] {
        let mut context = SecurityContext::from_resolved_identity("someone", "agent", None);
        context.principal.role = role.map(str::to_string);
        let request = engine
            .authorize(&context, "Add", "Note", &HashMap::new())
            .is_allowed();
        context.cause = Some(MoveCause::reaction(hop.clone(), None));
        let reaction = engine
            .authorize(&context, "Address", "Note", &HashMap::new())
            .is_allowed();
        let engine_answers =
            [request, reaction].map(|allowed| if allowed { May::Yes } else { May::No });
        // Its Add and its Address are answered for the same kind.
        assert!(
            kinds.iter().any(|kind| answers(kind) == engine_answers),
            "{role:?}: {engine_answers:?} in none of {:?}",
            kinds.iter().map(answers).collect::<Vec<_>>()
        );
    }
}

#[test]
fn like_errors_keep_the_senders_real_request_and_reaction_answers() {
    let policies = r#"
permit(principal, action == Action::"Add", resource is Note)
when { principal has flag };
forbid(principal, action == Action::"Add", resource is Note)
when { !(principal.flag like "x*") };
permit(principal, action == Action::"Address", resource is Note)
when { principal.flag like "x*" };
permit(principal, action == Action::"Other", resource is Note)
when { principal.flag like "y*" };
"#;
    let analysis = SendAnalysis::new(policies).unwrap();
    let engine = AuthzEngine::new(policies).unwrap();
    let actions = ["Add", "Address", "Other"];
    let questions: Vec<_> = actions
        .iter()
        .map(|action| Question {
            action,
            resource_type: "Note",
            cause: Cause::request(),
        })
        .collect();
    let kinds = analysis.outside_kinds(&questions, &|_| Ok(())).unwrap();
    let answers: Vec<Vec<May>> = kinds
        .iter()
        .map(|kind| {
            questions
                .iter()
                .map(|q| {
                    analysis.may(
                        &Asker::OutsideAs(kind.clone()),
                        q.action,
                        q.resource_type,
                        &BTreeMap::new(),
                        Some(&q.cause),
                    )
                })
                .collect()
        })
        .collect();
    for value in [
        None,
        Some(serde_json::json!(false)),
        Some(serde_json::json!(true)),
        Some(serde_json::json!(42)),
        Some(serde_json::json!([])),
        Some(serde_json::json!({"nested": "x"})),
        Some(serde_json::json!("xylophone")),
        Some(serde_json::json!("yes")),
        Some(serde_json::json!("neither")),
    ] {
        let mut context = SecurityContext::from_resolved_identity("someone", "agent", None);
        if let Some(value) = &value {
            context
                .principal
                .attributes
                .insert("flag".into(), value.clone());
        }
        let expected: Vec<_> = actions
            .iter()
            .map(|action| {
                if engine
                    .authorize(&context, action, "Note", &HashMap::new())
                    .is_allowed()
                {
                    May::Yes
                } else {
                    May::No
                }
            })
            .collect();
        assert!(
            answers.contains(&expected),
            "flag={value:?}: {expected:?} missing from {answers:?}"
        );
    }
    assert!(
        answers.iter().all(|answers| !answers.contains(&May::Maybe)),
        "{answers:?}"
    );
}

#[test]
fn combined_sender_checks_split_even_when_each_alone_leaves_maybe() {
    let analysis = SendAnalysis::new(
        r#"
permit(principal, action == Action::"Add", resource is Note)
when { (principal has role) == (principal has agent_type) };
permit(principal, action == Action::"Address", resource is Note)
when { (principal has role) == (principal has agent_type) && context.via != "request" };
"#,
    )
    .unwrap();
    let questions = [
        Question {
            action: "Add",
            resource_type: "Note",
            cause: Cause::request(),
        },
        Question {
            action: "Address",
            resource_type: "Note",
            cause: Cause::reaction(false, Hop::unknown()),
        },
    ];
    let answers: Vec<Vec<_>> = analysis
        .outside_kinds(&questions, &|_| Ok(()))
        .unwrap()
        .into_iter()
        .map(|kind| {
            questions
                .iter()
                .map(|q| {
                    analysis.may(
                        &Asker::OutsideAs(kind.clone()),
                        q.action,
                        q.resource_type,
                        &BTreeMap::new(),
                        Some(&q.cause),
                    )
                })
                .collect()
        })
        .collect();
    assert!(answers.contains(&vec![May::Yes, May::Yes]), "{answers:?}");
    assert!(answers.contains(&vec![May::No, May::No]), "{answers:?}");
    assert!(
        answers.iter().all(|answers| !answers.contains(&May::Maybe)),
        "{answers:?}"
    );
}

#[test]
fn a_denied_partition_reservation_never_returns_a_partial_cover() {
    let condition = (1..8).fold("(principal has flag0)".to_string(), |left, at| {
        format!("({left} == (principal has flag{at}))")
    });
    let analysis = SendAnalysis::new(&format!(
        "permit(principal, action, resource) when {{ {condition} }};"
    ))
    .unwrap();
    let calls = std::cell::Cell::new(0);
    let result = analysis.outside_kinds(
        &[Question {
            action: "Add",
            resource_type: "Note",
            cause: Cause::request(),
        }],
        &|bytes| {
            assert!(bytes > 0);
            calls.set(calls.get() + 1);
            if calls.get() == 3 {
                Err("test room is full".into())
            } else {
                Ok(())
            }
        },
    );
    assert_eq!(
        result.expect_err("denial must return no sender cover"),
        "test room is full"
    );
    assert_eq!(calls.get(), 3, "construction must stop at the first denial");
}

/// The steps a partition takes on these policies for one question, as its
/// reservations count them, and the kinds it finds.
fn partition_steps(policy: &str) -> (usize, usize) {
    let analysis = SendAnalysis::new(policy).unwrap();
    let calls = std::cell::Cell::new(0);
    let kinds = analysis
        .outside_kinds(
            &[Question {
                action: "Add",
                resource_type: "Note",
                cause: Cause::request(),
            }],
            &|_| {
                calls.set(calls.get() + 1);
                Ok(())
            },
        )
        .unwrap();
    (calls.get(), kinds.len())
}

#[test]
fn senders_a_policy_has_finished_with_do_not_multiply_the_kinds() {
    // Once one pair decides, or fails, what is left is the same whatever
    // made it fail: a tree of every value would take over 3^12 steps.
    let condition = (0..6)
        .map(|at| format!("(principal has a{at} && principal has b{at})"))
        .collect::<Vec<_>>()
        .join(" || ");
    let (steps, kinds) = partition_steps(&format!(
        "permit(principal, action, resource) when {{ {condition} }};"
    ));
    assert_eq!(kinds, 2, "may send, and may not");
    assert!(steps < 200, "took {steps} steps");
}

#[test]
fn a_literal_only_other_actions_compare_with_tells_no_senders_apart() {
    let (_, without) = partition_steps(
        r#"permit(principal, action == Action::"Add", resource is Note)
        when { principal has role && principal.role == "member" };"#,
    );
    let (_, with) = partition_steps(
        r#"permit(principal, action == Action::"Add", resource is Note)
        when { principal has role && principal.role == "member" };
        permit(principal, action == Action::"Other", resource is Note)
        when { principal has role && principal.role == "helper" };"#,
    );
    assert_eq!(without, with);
}

#[test]
fn questions_everyone_has_one_answer_to_leave_the_kinds_as_they_are() {
    // Anyone may Archive and no one may Delete: neither tells a member
    // from anyone else, and Add still does.
    let analysis = SendAnalysis::new(
        r#"permit(principal, action == Action::"Add", resource is Note)
        when { principal has role && principal.role == "member" };
        permit(principal, action == Action::"Archive", resource is Note);"#,
    )
    .expect("policies parse");
    let question = |action| Question {
        action,
        resource_type: "Note",
        cause: Cause::request(),
    };
    let unlimited = |_: usize| Ok(());
    let alone = analysis
        .outside_kinds(&[question("Add")], &unlimited)
        .expect("fits");
    let among = analysis
        .outside_kinds(
            &[question("Delete"), question("Archive"), question("Add")],
            &unlimited,
        )
        .expect("fits");
    assert_eq!(alone.len(), 2, "members, and the rest");
    assert_eq!(among, alone);
}

#[test]
fn the_same_questions_are_answered_once() {
    let analysis = SendAnalysis::new(
        r#"permit(principal, action, resource)
        when { principal has role && principal.role == "member" };"#,
    )
    .unwrap();
    let questions = [Question {
        action: "Add",
        resource_type: "Note",
        cause: Cause::request(),
    }];
    let calls = std::cell::Cell::new(0);
    let reserve = |_: usize| {
        calls.set(calls.get() + 1);
        Ok(())
    };
    let first = analysis.outside_kinds(&questions, &reserve).unwrap();
    let after_first = calls.get();
    let again = analysis.outside_kinds(&questions, &reserve).unwrap();
    assert_eq!(first, again);
    assert_eq!(
        calls.get(),
        after_first + 1,
        "only what it holds is reserved"
    );
}

#[test]
fn a_policy_reads_the_source_move_through_the_rule_the_move_or_the_whole_context() {
    for (when, reads) in [
        (r#"context.causeAction == "Drop""#, true),
        (r#"context.rule == "Ticket:Drop:ping""#, true),
        ("context has causeAction", true),
        (r#"context == {"via": "reaction"}"#, true),
        (r#"context.via != "request""#, false),
        (r#"context.causeEntityType == "Ticket""#, false),
        ("true", false),
    ] {
        let policies =
            format!(r#"permit(principal, action == Action::"Ping", resource) when {{ {when} }};"#);
        let analysis = SendAnalysis::new(&policies).expect("policies parse");
        assert_eq!(analysis.reads_source_move("Ping", "Board"), reads, "{when}");
        assert!(!analysis.reads_source_move("Other", "Board"), "{when}");
    }
}
