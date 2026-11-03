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
