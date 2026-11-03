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
            analysis.may(&asker, "Bring", "Place", &BTreeMap::new()),
            if denied { May::No } else { May::Yes },
            "{expression}"
        );
    }
}
