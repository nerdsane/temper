//! Every value is read in its TOML type and every key and section is known:
//! each case below used to load and silently mean something else.

use super::parser::{LivenessEnforcement, parse_automaton_with_liveness};
use super::types::{Automaton, TargetResolver};
use crate::predicate::Arg;

/// A valid spec with `state_extra` added to the `owner_id` variable,
/// `action_extra` to the `Finish` action, and `tail` appended.
fn spec(state_extra: &str, action_extra: &str, tail: &str) -> String {
    format!(
        r#"
[automaton]
name = "Doc"
states = ["Draft", "Done"]
initial = "Draft"

[[state]]
name = "count"
type = "counter"
initial = 0

[[state]]
name = "ready"
type = "bool"
initial = false

[[state]]
name = "owner_id"
type = "string"
initial = ""
{state_extra}

[[action]]
name = "Finish"
kind = "input"
from = ["Draft"]
to = "Done"
params = ["note"]
{action_extra}
{tail}
"#
    )
}

fn parse(source: &str) -> Result<Automaton, String> {
    parse_automaton_with_liveness(source, LivenessEnforcement::WarnOnly).map_err(|e| e.to_string())
}

#[track_caller]
fn rejects(source: &str, needle: &str) {
    match parse(source) {
        Ok(_) => panic!("accepted a spec that should fail with `{needle}`:\n{source}"),
        Err(error) => assert!(
            error.contains(needle),
            "error does not mention `{needle}`: {error}"
        ),
    }
}

const ENTITY_TRIGGER: &str = r#"
[[action.triggers]]
name = "notify"
kind = "entity"
target_entity = "Owner"
target_action = "Notify"
resolve_target = { kind = "field", id_field = "owner_id" }
"#;

#[test]
fn base_spec_loads() {
    parse(&spec("", "", "")).expect("base spec");
}

#[test]
fn values_use_their_toml_type() {
    rejects(&spec(r#"query_indexed = "no""#, "", ""), "query_indexed");
    rejects(&spec(r#"query_indexed = "false""#, "", ""), "query_indexed");
    rejects(
        &spec(r#"overflow_ttl_seconds = "1h""#, "", ""),
        "overflow_ttl_seconds",
    );
    rejects(
        &spec(r#"overflow_inline_max_bytes = "4096""#, "", ""),
        "overflow_inline_max_bytes",
    );
    rejects(
        &spec(
            "",
            "",
            "[[liveness]]\nname = \"L\"\nfrom = [\"Draft\"]\nhas_actions = \"yes\"",
        ),
        "has_actions",
    );
    rejects(
        &spec(
            "",
            "",
            "[[liveness]]\nname = \"L\"\nfrom = [\"Draft\"]\nhas_actions = \"true\"",
        ),
        "has_actions",
    );
    rejects(
        &spec("", "", "").replace(
            "initial = \"Draft\"",
            "initial = \"Draft\"\nstrict_action_params = \"true\"",
        ),
        "strict_action_params",
    );
    rejects(
        &spec("", "", "").replace("from = [\"Draft\"]", "from = \"Draft\""),
        "from",
    );
}

#[test]
fn initial_is_required_and_typed() {
    rejects(
        &spec("", "", "").replace("initial = 0", "initial = \"0\""),
        "initial",
    );
    rejects(
        &spec("", "", "").replace("initial = false", "initial = \"false\""),
        "initial",
    );
    rejects(
        &spec("", "", "").replace("initial = 0", "initial = -1"),
        "initial",
    );
    rejects(
        &spec("", "", "").replace("initial = \"\"\n", "\n"),
        "initial",
    );
    rejects(
        &spec(
            "",
            "",
            "[[state]]\nname = \"tags\"\ntype = \"list\"\ninitial = \"[]\"",
        ),
        "initial",
    );
}

#[test]
fn one_type_vocabulary() {
    for removed in ["float", "number", "integer", "set", "status", "uint64"] {
        rejects(
            &spec(
                "",
                "",
                &format!("[[state]]\nname = \"x\"\ntype = \"{removed}\"\ninitial = \"\""),
            ),
            removed,
        );
    }
    rejects(
        &spec("", "", "").replace(
            r#"params = ["note"]"#,
            r#"params = [{ name = "n", type = "uint64" }]"#,
        ),
        "uint64",
    );
}

#[test]
fn unknown_keys_and_sections_are_errors() {
    rejects(&spec("", r#"hnit = "Finish the doc.""#, ""), "hnit");
    rejects(&spec("bogus = 1", "", ""), "bogus");
    rejects(&spec("", "", "[extras]\nx = 1"), "extras");
    rejects(
        &spec("", "", "[automaton.timeouts]\nDraft = \"120s\""),
        "timeouts",
    );
    rejects(
        &spec(
            "",
            "",
            "[[state_timeout]]\nstate = \"Draft\"\nafter_seconds = 5\non_timeout = \"Finish\"\nbogus = 1",
        ),
        "bogus",
    );
    rejects(
        &spec(
            "",
            "",
            "[[invariant]]\nname = \"I\"\nassert = \"count >= 0\"\nmessage = \"x\"",
        ),
        "message",
    );
    rejects(
        &spec(
            "",
            "",
            "[[field_invariant]]\nname = \"F\"\nassert = \"true\"\nbogus = 1",
        ),
        "bogus",
    );
    rejects(
        &spec(
            "",
            "",
            "[[key]]\nname = \"k\"\nproperties = [\"owner_id\"]\nbogus = 1",
        ),
        "bogus",
    );
    rejects(
        &spec("", "", "[admission]\nqueue_depth = 5\nbogus = 1"),
        "bogus",
    );
}

#[test]
fn entries_need_a_name() {
    rejects(
        &spec("", "", "[[state]]\ntype = \"bool\"\ninitial = false"),
        "name",
    );
    rejects(
        &spec(
            "",
            "",
            "[[state]]\nname = \"\"\ntype = \"bool\"\ninitial = false",
        ),
        "name",
    );
    rejects(
        &spec("", "", "[[action]]\nkind = \"input\"\nfrom = [\"Draft\"]"),
        "name",
    );
}

#[test]
fn action_kind_is_a_closed_lowercase_set() {
    rejects(
        &spec("", "", "").replace("kind = \"input\"", "kind = \"inptu\""),
        "inptu",
    );
    rejects(
        &spec("", "", "").replace("kind = \"input\"", "kind = \"Composite\""),
        "Composite",
    );
    rejects(&spec("", "record_parent_event = false", ""), "composite");
    rejects(
        &spec(
            "",
            "",
            "[[action.sub_writes]]\ntarget_entity = \"Line\"\naction = \"Create\"",
        ),
        "composite",
    );
    let composite = spec("", "record_parent_event = false", "")
        .replace("kind = \"input\"", "kind = \"composite\"");
    let parsed = parse(&composite).expect("composite action");
    assert!(!parsed.actions[0].record_parent_event);
    rejects(
        &spec("", "record_parent_event = \"no\"", "")
            .replace("kind = \"input\"", "kind = \"composite\""),
        "record_parent_event",
    );
}

#[test]
fn triggers_use_args_guard_and_id_field() {
    let source = spec(
        "",
        "",
        &format!(
            "{ENTITY_TRIGGER}guard = \"status == 'Done' && ready\"\nargs = {{ source = \"'doc'\", doc_id = \"Id\", owner = \"owner_id\", note = \"note\", n = \"3\", flag = \"true\", gone = \"null\" }}\n"
        ),
    );
    let parsed = parse(&source).expect("entity trigger");
    let trigger = &parsed.actions[0].triggers[0];
    assert_eq!(
        trigger.resolve_target,
        Some(TargetResolver::Field {
            id_field: "owner_id".into()
        })
    );
    assert_eq!(trigger.args["doc_id"], Arg::Var("Id".into()));
    assert_eq!(trigger.args["owner"], Arg::Var("owner_id".into()));
    assert_eq!(trigger.args["note"], Arg::Var("note".into()));
    assert_eq!(trigger.args["source"].to_string(), "'doc'");
    assert_eq!(trigger.args["n"].to_string(), "3");
    assert_eq!(trigger.args["flag"].to_string(), "true");
    assert_eq!(trigger.args["gone"].to_string(), "null");
    assert_eq!(trigger.status_filter(), Some(vec!["Done".to_string()]));
}

#[test]
fn retired_trigger_keys_are_errors() {
    rejects(
        &spec("", "", &format!("{ENTITY_TRIGGER}to_state = \"Done\"")),
        "to_state",
    );
    rejects(
        &spec("", "", &format!("{ENTITY_TRIGGER}params = {{ a = \"b\" }}")),
        "params",
    );
    rejects(
        &spec(
            "",
            "",
            &format!("{ENTITY_TRIGGER}params_from = {{ a = \"owner_id\" }}"),
        ),
        "params_from",
    );
    rejects(
        &spec("", "", ENTITY_TRIGGER)
            .replace("{ kind = \"field\", id_field", "{ type = \"field\", field"),
        "type",
    );
    rejects(
        &spec("", "", ENTITY_TRIGGER).replace("id_field = \"owner_id\"", "field = \"owner_id\""),
        "field",
    );
    rejects(
        &spec(
            "",
            "",
            "[[action.triggers]]\nname = \"run\"\nkind = \"adapter\"\nadapter_type = \"codex\"",
        ),
        "adapter_type",
    );
}

#[test]
fn args_names_are_checked() {
    rejects(
        &spec(
            "",
            "",
            &format!("{ENTITY_TRIGGER}args = {{ source = \"doc\" }}"),
        ),
        "doc",
    );
    rejects(
        &spec(
            "",
            "",
            &format!("{ENTITY_TRIGGER}args = {{ source = \"params.note\" }}"),
        ),
        "params.note",
    );
    rejects(
        &spec(
            "",
            "",
            &format!("{ENTITY_TRIGGER}args = {{ source = \"Owner[owner_id].status\" }}"),
        ),
        "args",
    );
    rejects(
        &spec(
            "",
            "",
            "[[state_timeout]]\nstate = \"Draft\"\nafter_seconds = 5\non_timeout = \"Finish\"\nargs = { note = \"note\" }",
        ),
        "note",
    );
    let timeout = spec(
        "",
        "",
        "[[state_timeout]]\nstate = \"Draft\"\nafter_seconds = 5\non_timeout = \"Finish\"\nargs = { note = \"'late'\", who = \"owner_id\" }",
    );
    let parsed = parse(&timeout).expect("state timeout args");
    assert_eq!(
        parsed.state_timeouts[0].args["who"],
        Arg::Var("owner_id".into())
    );
    rejects(
        &spec(
            "",
            "",
            "[[state_timeout]]\nstate = \"Draft\"\nafter_seconds = 5\non_timeout = \"Finish\"\nparams = { note = \"late\" }",
        ),
        "params",
    );
}

#[test]
fn trigger_keys_belong_to_their_kind() {
    rejects(
        &spec("", "", &format!("{ENTITY_TRIGGER}module = \"m\"")),
        "module",
    );
    rejects(
        &spec(
            "",
            "",
            "[[action.triggers]]\nname = \"run\"\nkind = \"wasm\"\nmodule = \"m\"\ntarget_entity = \"Owner\"",
        ),
        "target_entity",
    );
    rejects(
        &spec(
            "",
            "",
            "[[action.triggers]]\nname = \"run\"\nkind = \"wasm\"\nmodule = \"m\"\nguard = \"ready\"",
        ),
        "guard",
    );
}

#[test]
fn webhooks_name_the_entity_id_source() {
    let webhook = "[[webhook]]\nname = \"cb\"\npath = \"cb\"\naction = \"Finish\"\nentity_id = \"query.state\"\nhmac_secret = \"{secret:CB}\"\nhmac_header = \"x-sig\"\n[webhook.extract]\ncode = \"query.code\"";
    let parsed = parse(&spec("", "", webhook)).expect("webhook");
    assert_eq!(parsed.webhooks[0].entity_id, "query.state");
    rejects(
        &spec(
            "",
            "",
            &webhook.replace(
                "entity_id = \"query.state\"",
                "entity_lookup = \"query_param\"\nentity_param = \"state\"",
            ),
        ),
        "entity_lookup",
    );
    rejects(
        &spec(
            "",
            "",
            &webhook.replace("entity_id = \"query.state\"\n", ""),
        ),
        "entity_id",
    );
    rejects(
        &spec("", "", &webhook.replace("\"query.code\"", "\"code\"")),
        "query.",
    );
    // The body and headers are sources too (ADR-0182).
    let from_body = parse(&spec(
        "",
        "",
        &webhook.replace("\"query.state\"", "\"body.data.object.id\""),
    ))
    .expect("body source");
    assert_eq!(from_body.webhooks[0].entity_id, "body.data.object.id");
    rejects(
        &spec(
            "",
            "",
            &webhook.replace("\"query.state\"", "\"form.state\""),
        ),
        "query.<name>, header.<name>, body or body.<path>",
    );
}

#[test]
fn authenticated_parameter_source_is_preserved_by_the_strict_reader() {
    let source = r#"
[automaton]
name="Owned"
states=["Requested","Active"]
initial="Requested"
strict_action_params=true
[[state]]
name="owner"
type="string"
initial=""
[[action]]
name="Create"
from=["Requested"]
to="Active"
params=[{name="owner",type="string",source="authenticated_subject"}]
"#;
    let auto = super::parse_automaton(source).expect("trusted parameter source must parse");
    let json = serde_json::to_value(&auto.actions[0].params[0]).unwrap();
    assert_eq!(json["source"], "authenticated_subject");
}

/// `declaration` is the bound parameter's declaration; `strict` the automaton's.
fn bound_label_spec(strict: bool, declaration: &str) -> String {
    format!(
        r#"
[automaton]
name="Post"
states=["Draft","Posted"]
initial="Draft"
strict_action_params={strict}
[[state]]
name="author"
type="string"
initial=""
[[action]]
name="Publish"
from=["Draft"]
to="Posted"
params=[{declaration}]
"#
    )
}

#[test]
fn an_agent_label_source_parses_on_a_strict_string_parameter() {
    let source = bound_label_spec(
        true,
        r#"{name="author",type="string",source="authenticated_agent_label"}"#,
    );
    let auto = super::parse_automaton(&source).expect("trusted parameter source must parse");
    let json = serde_json::to_value(&auto.actions[0].params[0]).unwrap();
    assert_eq!(json["source"], "authenticated_agent_label");
}

#[test]
fn an_agent_label_source_needs_strict_params_and_a_string() {
    for source in [
        bound_label_spec(
            false,
            r#"{name="author",type="string",source="authenticated_agent_label"}"#,
        ),
        bound_label_spec(
            true,
            r#"{name="author",type="bool",source="authenticated_agent_label"}"#,
        ),
    ] {
        let error = super::parse_automaton(&source).unwrap_err().to_string();
        assert!(
            error.contains("requires strict_action_params and string type"),
            "{error}"
        );
    }
}

#[test]
fn a_related_record_s_field_is_read_only_by_an_invariant() {
    let invariant = r#"
[[invariant]]
name = "same_note"
assert = "status == 'Done' => Owner[owner_id].owner_id == owner_id"
"#;
    parse(&spec("", "", invariant)).expect("an invariant may read a related field");
    rejects(
        &spec("", "guard = \"Owner[owner_id].owner_id == owner_id\"", ""),
        "only an [[invariant]] may do",
    );
}
