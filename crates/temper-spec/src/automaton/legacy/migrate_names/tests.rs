use super::super::migrate_source;
use crate::automaton::{
    LivenessEnforcement, TargetResolver, WebhookSignature, parse_automaton_with_liveness,
};
use crate::predicate::Arg;

/// A spec in every old spelling this pass rewrites.
const OLD: &str = r#"
[automaton]
name = "Doc"
states = ["Draft", "Done"]
initial = "Draft"
strict_action_params = "false"

[automaton.timeouts]
Draft = "120s"

[[state]]
name = "count"
type = "counter"
initial = "3"

[[state]]
name = "ready"
type = "bool"
initial = "false"

[[state]]
name = "tags"
type = "set"
initial = "[a, b]"

[[state]]
name = "owner_id"
type = "string"
initial = ""
query_indexed = "false"
overflow_ttl_seconds = "1h"

[[state]]
name = "unnamed_type"
initial = 7

[[action]]
name = "Finish"
kind = "Composite"
from = "Draft"
to = "Done"
params = ["note", { name = "seq", type = "uint64" }]
record_parent_event = "false"
hnit = "typo"

[[action.triggers]]
name = "notify"
kind = "entity"
to_state = "Done"
guard = "ready"
target_entity = "Owner"
target_action = "Notify"
params = { source = "doc", n = 2 }
module = "stray"

[action.triggers.params_from]
doc_id = "Id"
owner = "owner_id"

[action.triggers.resolve_target]
type = "field"
field = "owner_id"

[[action.triggers]]
name = "run"
kind = "adapter"
adapter_type = "codex"
guard = "ready"

[[action]]
name = "Plain"
kind = "input"
from = ["Draft"]
record_parent_event = false

[[state_timeout]]
state = "Draft"
after_seconds = 60
on_timeout = "Finish"
params = { error_message = "Draft took too long." }

[[webhook]]
name = "cb"
path = "cb"
action = "Finish"
entity_lookup = "query_param"
entity_param = "state"
signature = "hmac"
hmac_secret = "{secret:CB}"
hmac_header = "x-sig"

[webhook.extract]
code = "code"
"#;

fn migrated() -> (String, Vec<String>) {
    let migration = migrate_source(OLD).unwrap_or_else(|e| panic!("migrate: {e}"));
    (migration.source, migration.notes)
}

#[test]
fn old_spellings_become_current() {
    let (source, notes) = migrated();
    let auto = parse_automaton_with_liveness(&source, LivenessEnforcement::WarnOnly)
        .unwrap_or_else(|e| panic!("{e}\n{source}"));

    assert!(!auto.automaton.strict_action_params);
    let initial = |name: &str| {
        auto.state
            .iter()
            .find(|s| s.name == name)
            .map(|s| s.initial.to_string())
            .unwrap()
    };
    assert_eq!(initial("count"), "3");
    assert_eq!(initial("ready"), "false");
    assert_eq!(initial("tags"), r#"["a","b"]"#);
    assert_eq!(initial("unnamed_type"), r#""7""#);
    let owner = auto.state.iter().find(|s| s.name == "owner_id").unwrap();
    assert_eq!(owner.query_indexed, Some(false));
    assert_eq!(owner.overflow_ttl_seconds, None);

    let finish = &auto.actions[0];
    assert_eq!(finish.kind.as_str(), "composite");
    assert_eq!(finish.from, vec!["Draft".to_string()]);
    assert!(!finish.record_parent_event);
    assert_eq!(finish.params[1].param_type().as_str(), "counter");

    let notify = &finish.triggers[0];
    assert_eq!(
        notify.guard.as_ref().unwrap().to_string(),
        "status == 'Done' && ready"
    );
    assert_eq!(
        notify.args["source"],
        Arg::Lit(crate::predicate::Literal::Str("doc".into()))
    );
    assert_eq!(notify.args["n"].to_string(), "2");
    assert_eq!(notify.args["doc_id"], Arg::Var("Id".into()));
    assert_eq!(notify.args["owner"], Arg::Var("owner_id".into()));
    assert_eq!(
        notify.resolve_target,
        Some(TargetResolver::Field {
            id_field: "owner_id".into()
        })
    );
    assert_eq!(notify.module, None);

    let run = &finish.triggers[1];
    assert_eq!(run.adapter.as_deref(), Some("codex"));
    assert_eq!(run.guard, None);

    assert!(
        auto.actions[1].record_parent_event,
        "dropped, so the default"
    );
    assert_eq!(
        auto.state_timeouts[0].args["error_message"].to_string(),
        "'Draft took too long.'"
    );
    assert_eq!(auto.webhooks[0].entity_id, "query.state");
    assert_eq!(auto.webhooks[0].extract["code"], "query.code");
    assert_eq!(
        auto.webhooks[0].signature,
        WebhookSignature::Hmac,
        "the provider's scheme survives migration"
    );

    for expected in [
        "[automaton.timeouts]",
        "overflow_ttl_seconds",
        "`hnit`",
        "`module`",
        "`guard` (adapter triggers",
        "action 'Plain': dropped record_parent_event",
    ] {
        assert!(
            notes.iter().any(|note| note.contains(expected)),
            "no note mentions {expected}: {notes:#?}"
        );
    }
}

#[test]
fn keeps_the_authors_layout() {
    let (source, _) = migrated();
    assert!(source.contains("[action.triggers.args]"), "{source}");
    assert!(
        source.contains("args = { error_message = \"'Draft took too long.'\" }"),
        "{source}"
    );
    assert!(!source.contains("params_from"), "{source}");
}

#[test]
fn migrating_twice_changes_nothing() {
    let (once, _) = migrated();
    let twice = migrate_source(&once).unwrap();
    assert_eq!(once, twice.source);
    assert!(twice.notes.is_empty(), "{:?}", twice.notes);
}

#[test]
fn unconvertible_values_are_errors() {
    let with = |from: &str, to: &str| {
        assert!(OLD.contains(from));
        migrate_source(&OLD.replace(from, to))
    };
    assert!(with("type = \"set\"", "type = \"float\"").is_err());
    assert!(with("source = \"doc\"", "source = \"it's\"").is_err());
    assert!(with("owner = \"owner_id\"", "owner = \"owner-id\"").is_err());
    assert!(
        with(
            "{ name = \"seq\", type = \"uint64\" }",
            "{ name = \"seq\", type = \"json\" }"
        )
        .is_err()
    );
}

#[test]
fn only_old_syntax_gets_the_migrate_hint() {
    let (current, _) = migrated();
    let typo = current.replace(
        "[[action]]\nname = \"Plain\"",
        "[[action]]\nname = \"Plain\"\nhnit = \"x\"",
    );
    let error = parse_automaton_with_liveness(&typo, LivenessEnforcement::WarnOnly)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("hnit") && !error.contains("migrate-predicates"),
        "{error}"
    );

    let old = current.replace("initial = 3", "initial = \"3\"");
    let error = parse_automaton_with_liveness(&old, LivenessEnforcement::WarnOnly)
        .unwrap_err()
        .to_string();
    assert!(error.contains("migrate-predicates"), "{error}");
}
