//! `says` on actions and `[[group]]` sections: what a statechart is drawn
//! from. Neither changes what the spec does.

use super::super::*;

const TRIP: &str = r#"
[automaton]
name = "Trip"
states = ["Idea", "Planning", "Booked", "Away", "Home", "Dropped"]
initial = "Idea"

[[action]]
name = "Plan"
kind = "input"
from = ["Idea"]
to = "Planning"

[[action]]
name = "Book"
kind = "input"
from = ["Planning"]
to = "Booked"

[[action]]
name = "Leave"
kind = "input"
from = ["Booked"]
to = "Away"

[[action]]
name = "Return"
kind = "input"
from = ["Away"]
to = "Home"

[[action]]
name = "Drop"
kind = "input"
from = ["Idea", "Planning"]
to = "Dropped"
"#;

/// `TRIP` with `extra` appended (TOML tables may follow the actions).
fn trip_with(extra: &str) -> String {
    format!("{TRIP}\n{extra}")
}

/// `TRIP` with `line` added to the `Plan` action.
fn trip_with_plan_line(line: &str) -> String {
    TRIP.replacen(
        "name = \"Plan\"\n",
        &format!("name = \"Plan\"\n{line}\n"),
        1,
    )
}

fn rejection(spec: &str) -> String {
    match parse_automaton(spec) {
        Ok(_) => panic!("spec should be refused:\n{spec}"),
        Err(error) => error.to_string(),
    }
}

#[test]
fn a_spec_without_the_keys_has_no_labels_and_no_groups() {
    let trip = parse_automaton(TRIP).expect("plain spec parses");
    assert!(trip.actions.iter().all(|action| action.says.is_none()));
    assert!(trip.groups.is_empty());
}

#[test]
fn an_action_carries_what_it_says() {
    let trip = parse_automaton(&trip_with_plan_line("says = \"start planning\""))
        .expect("says is an optional action key");
    let plan = trip.actions.iter().find(|a| a.name == "Plan").unwrap();
    assert_eq!(plan.says.as_deref(), Some("start planning"));
    let book = trip.actions.iter().find(|a| a.name == "Book").unwrap();
    assert_eq!(book.says, None);
}

#[test]
fn says_changes_nothing_the_action_does() {
    let plain = parse_automaton(TRIP).unwrap();
    let labelled = parse_automaton(&trip_with_plan_line("says = \"start planning\"")).unwrap();
    let plain_plan = plain.actions.iter().find(|a| a.name == "Plan").unwrap();
    let plan = labelled.actions.iter().find(|a| a.name == "Plan").unwrap();
    assert_eq!(plan.from, plain_plan.from);
    assert_eq!(plan.to, plain_plan.to);
    assert_eq!(plan.kind, plain_plan.kind);
    assert_eq!(
        format!("{:?}", to_state_machine(&labelled)),
        format!("{:?}", to_state_machine(&plain)),
        "the verified state machine must not see the label"
    );
}

#[test]
fn an_empty_says_is_refused() {
    let error = rejection(&trip_with_plan_line("says = \"\""));
    assert!(error.contains("action 'Plan'"), "{error}");
    assert!(error.contains("says must not be empty"), "{error}");
}

#[test]
fn a_blank_says_is_refused() {
    let error = rejection(&trip_with_plan_line("says = \"   \""));
    assert!(error.contains("says must not be empty"), "{error}");
}

#[test]
fn a_long_says_over_several_lines_is_kept_as_written() {
    // The reader wraps labels, so a label has no length or line limit.
    let long = "start planning: pick dates, ask who comes, and find a place to stay\\nthen wait for answers";
    let trip = parse_automaton(&trip_with_plan_line(&format!("says = \"{long}\"")))
        .expect("says has no length or line limit");
    let plan = trip.actions.iter().find(|a| a.name == "Plan").unwrap();
    assert_eq!(
        plan.says.as_deref(),
        Some(long.replace("\\n", "\n").as_str())
    );
}

#[test]
fn each_block_of_an_action_keeps_its_own_says() {
    let trip = parse_automaton(&trip_with(
        r#"
[[action]]
name = "Rethink"
kind = "input"
from = ["Booked"]
to = "Planning"
says = "change the plan"

[[action]]
name = "Rethink"
kind = "input"
from = ["Away"]
to = "Booked"
says = "come back early"
"#,
    ))
    .expect("an action may be written as several blocks");
    let says: Vec<(Vec<String>, Option<&str>)> = trip
        .actions
        .iter()
        .filter(|a| a.name == "Rethink")
        .map(|a| (a.from.clone(), a.says.as_deref()))
        .collect();
    assert_eq!(
        says,
        vec![
            (vec!["Booked".to_string()], Some("change the plan")),
            (vec!["Away".to_string()], Some("come back early")),
        ]
    );
}

#[test]
fn says_must_be_text() {
    let error = rejection(&trip_with_plan_line("says = 3"));
    assert!(error.contains("says"), "{error}");
}

#[test]
fn groups_are_read_with_their_states() {
    let trip = parse_automaton(&trip_with(
        r#"
[[group]]
name = "Getting ready"
says = "before the trip"
states = ["Planning", "Booked"]

[[group]]
name = "Going"
states = ["Away", "Home"]
"#,
    ))
    .expect("groups of declared states parse");
    assert_eq!(trip.groups.len(), 2);
    assert_eq!(trip.groups[0].name, "Getting ready");
    assert_eq!(trip.groups[0].says.as_deref(), Some("before the trip"));
    assert_eq!(trip.groups[0].states, vec!["Planning", "Booked"]);
    assert_eq!(trip.groups[1].name, "Going");
    assert_eq!(trip.groups[1].says, None);
}

#[test]
fn a_group_may_hold_other_groups() {
    let trip = parse_automaton(&trip_with(
        r#"
[[group]]
name = "The trip"
says = "the whole trip"
states = ["Getting ready", "Going"]

[[group]]
name = "Getting ready"
states = ["Planning", "Booked"]

[[group]]
name = "Going"
states = ["Away", "Home"]
"#,
    ))
    .expect("a group may list groups declared after it");
    let outer = trip.groups.iter().find(|g| g.name == "The trip").unwrap();
    assert_eq!(outer.states, vec!["Getting ready", "Going"]);
}

#[test]
fn groups_change_nothing_the_spec_does() {
    let plain = parse_automaton(TRIP).unwrap();
    let grouped = parse_automaton(&trip_with(
        "[[group]]\nname = \"Getting ready\"\nstates = [\"Planning\", \"Booked\"]\n",
    ))
    .unwrap();
    assert_eq!(grouped.automaton.states, plain.automaton.states);
    assert_eq!(
        format!("{:?}", to_state_machine(&grouped)),
        format!("{:?}", to_state_machine(&plain)),
        "the verified state machine must not see the groups"
    );
}

#[test]
fn a_group_entry_must_be_a_state_or_a_group() {
    let error = rejection(&trip_with(
        "[[group]]\nname = \"Getting ready\"\nstates = [\"Planning\", \"Packing\"]\n",
    ));
    assert!(error.contains("group 'Getting ready'"), "{error}");
    assert!(error.contains("'Packing'"), "{error}");
    assert!(error.contains("not a declared state or group"), "{error}");
}

#[test]
fn a_state_in_two_groups_is_refused() {
    let error = rejection(&trip_with(
        r#"
[[group]]
name = "Getting ready"
states = ["Planning", "Booked"]

[[group]]
name = "Paid"
states = ["Booked", "Away"]
"#,
    ));
    assert!(error.contains("'Booked'"), "{error}");
    assert!(error.contains("at most one group"), "{error}");
}

#[test]
fn a_state_listed_twice_in_one_group_is_refused() {
    let error = rejection(&trip_with(
        "[[group]]\nname = \"Getting ready\"\nstates = [\"Planning\", \"Planning\"]\n",
    ));
    assert!(error.contains("'Planning'"), "{error}");
    assert!(error.contains("at most one group"), "{error}");
}

#[test]
fn a_state_in_a_group_and_in_the_group_around_it_is_refused() {
    let error = rejection(&trip_with(
        r#"
[[group]]
name = "The trip"
states = ["Getting ready", "Planning"]

[[group]]
name = "Getting ready"
states = ["Planning", "Booked"]
"#,
    ));
    assert!(error.contains("'Planning'"), "{error}");
    assert!(error.contains("at most one group"), "{error}");
}

#[test]
fn a_group_inside_two_groups_is_refused() {
    let error = rejection(&trip_with(
        r#"
[[group]]
name = "Early"
states = ["Getting ready"]

[[group]]
name = "Paid"
states = ["Getting ready"]

[[group]]
name = "Getting ready"
states = ["Planning", "Booked"]
"#,
    ));
    assert!(error.contains("'Getting ready'"), "{error}");
    assert!(error.contains("at most one group"), "{error}");
}

#[test]
fn group_names_are_unique() {
    let error = rejection(&trip_with(
        r#"
[[group]]
name = "Getting ready"
states = ["Planning"]

[[group]]
name = "Getting ready"
states = ["Booked"]
"#,
    ));
    assert!(error.contains("group 'Getting ready'"), "{error}");
    assert!(error.contains("declared twice"), "{error}");
}

#[test]
fn a_group_may_not_take_a_state_name() {
    let error = rejection(&trip_with(
        "[[group]]\nname = \"Away\"\nstates = [\"Booked\"]\n",
    ));
    assert!(error.contains("group 'Away'"), "{error}");
    assert!(error.contains("name of a state"), "{error}");
}

#[test]
fn a_group_inside_itself_is_refused() {
    let error = rejection(&trip_with(
        "[[group]]\nname = \"Loop\"\nstates = [\"Loop\", \"Planning\"]\n",
    ));
    assert!(error.contains("cycle"), "{error}");
    assert!(error.contains("'Loop'"), "{error}");
}

#[test]
fn groups_inside_each_other_are_refused() {
    let error = rejection(&trip_with(
        r#"
[[group]]
name = "A"
states = ["B", "Planning"]

[[group]]
name = "B"
states = ["C"]

[[group]]
name = "C"
states = ["A", "Booked"]
"#,
    ));
    assert!(error.contains("cycle"), "{error}");
    for name in ["'A'", "'B'", "'C'"] {
        assert!(error.contains(name), "{error}");
    }
}

#[test]
fn a_group_says_follows_the_action_rule() {
    let empty = rejection(&trip_with(
        "[[group]]\nname = \"Getting ready\"\nsays = \" \"\nstates = [\"Planning\"]\n",
    ));
    assert!(empty.contains("group 'Getting ready'"), "{empty}");
    assert!(empty.contains("says must not be empty"), "{empty}");
}

#[test]
fn a_group_with_no_states_is_refused() {
    for spec in [
        trip_with("[[group]]\nname = \"Getting ready\"\nstates = []\n"),
        trip_with("[[group]]\nname = \"Getting ready\"\n"),
    ] {
        let error = rejection(&spec);
        assert!(error.contains("group 'Getting ready'"), "{error}");
        assert!(error.contains("lists no states"), "{error}");
    }
}

#[test]
fn a_group_takes_only_its_own_keys() {
    let error = rejection(&trip_with(
        "[[group]]\nname = \"Getting ready\"\nstates = [\"Planning\"]\ncolor = \"red\"\n",
    ));
    assert!(error.contains("group 'Getting ready'"), "{error}");
    assert!(error.contains("color"), "{error}");
}

#[test]
fn a_group_name_in_from_is_still_an_undeclared_state() {
    let error = rejection(
        &trip_with("[[group]]\nname = \"Getting ready\"\nstates = [\"Planning\", \"Booked\"]\n")
            .replace("from = [\"Away\"]", "from = [\"Getting ready\"]"),
    );
    assert!(
        error.contains("undeclared from-state 'Getting ready'"),
        "{error}"
    );
}

#[test]
fn a_group_name_in_to_is_still_an_undeclared_state() {
    let error = rejection(
        &trip_with("[[group]]\nname = \"Going\"\nstates = [\"Away\", \"Home\"]\n")
            .replace("to = \"Away\"", "to = \"Going\""),
    );
    assert!(error.contains("undeclared to-state 'Going'"), "{error}");
}
