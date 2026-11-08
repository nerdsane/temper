//! Rules over two records, checked by induction with Z3: small models with a
//! known answer. Each model starts with the defect; the `*_FIX` text puts it
//! right by adding a reaction to the spec's last move.

use std::collections::BTreeMap;

use temper_spec::automaton::{Automaton, parse_automaton};
use temper_verify::smt::cross::{
    CrossFact, CrossOptions, CrossVerdict, check_cross_rule, check_cross_rules, cross_rules,
    link_rules,
};

const JOB: &str = include_str!("cross_models/chain/Job.ioa.toml");
const HOME: &str = include_str!("cross_models/chain/Home.ioa.toml");
const KICK: &str = include_str!("cross_models/chain/Kick.ioa.toml");
const JOB_FIX: &str = r#"
[[action.triggers]]
name = "homed"
kind = "entity"
target_entity = "Job"
target_action = "Rehome"
guard = "empty(home) || !(Home[home].status in ['Open'])"
[action.triggers.args]
home = "'main'"
[action.triggers.resolve_target]
kind = "same_id"
"#;

const POST: &str = include_str!("cross_models/post/Post.ioa.toml");
const THREAD: &str = include_str!("cross_models/post/Thread.ioa.toml");
const POST_FIX: &str = r#"
[[action.triggers]]
name = "make_thread"
kind = "entity"
target_entity = "Thread"
target_action = "Add"
guard = "!empty(thread) && Thread[thread].status == 'Unmade'"
[action.triggers.resolve_target]
kind = "field"
id_field = "thread"
"#;

const BOX: &str = include_str!("cross_models/boxes/Box.ioa.toml");
const LID: &str = include_str!("cross_models/boxes/Lid.ioa.toml");

const ALARM: &str = include_str!("cross_models/race/Alarm.ioa.toml");
const CASE: &str = include_str!("cross_models/race/Case.ioa.toml");
const CASE_FIX: &str = r#"
[[action.triggers]]
name = "alarm_cleared"
kind = "entity"
target_entity = "Case"
target_action = "Close"
guard = "Alarm[parent_id].status == 'Cleared'"
[action.triggers.resolve_target]
kind = "same_id"
"#;

fn specs(texts: &[String]) -> BTreeMap<String, Automaton> {
    texts
        .iter()
        .map(|text| {
            let spec = parse_automaton(text).expect("a test model parses");
            (spec.automaton.name.clone(), spec)
        })
        .collect()
}

fn fixed(text: &str, fix: &str) -> String {
    format!("{text}{fix}")
}

/// Someone outside may send these moves, and every other move is a reaction.
fn outside_sends(moves: &'static [&'static str]) -> impl Fn(&str, &str) -> bool {
    move |_, action| moves.contains(&action)
}

fn chain_facts() -> Vec<CrossFact> {
    vec![CrossFact {
        ty: "Home".into(),
        id: "main".into(),
        statuses: vec!["Open".into()],
    }]
}

/// The one app rule of `specs`, with the link candidates assumed.
fn verdict(specs: &BTreeMap<String, Automaton>, options: &CrossOptions<'_>) -> CrossVerdict {
    let mut rules = cross_rules(specs);
    assert_eq!(rules.len(), 1, "a test model holds one rule");
    let name = format!("{}.{}", rules[0].owner, rules[0].name);
    rules.extend(link_rules(specs));
    check_cross_rules(specs, &rules, options)
        .into_iter()
        .find(|verdict| verdict.rule == name)
        .expect("the rule has a verdict")
}

#[test]
fn a_break_reads_as_the_whole_path_from_a_real_start() {
    let may_send = outside_sends(&["Add", "Go"]);
    let options = CrossOptions {
        facts: chain_facts(),
        may_send: Some(&may_send),
        ..CrossOptions::default()
    };
    let verdict = verdict(&specs(&[JOB.into(), HOME.into(), KICK.into()]), &options);
    assert!(!verdict.proved);
    assert!(!verdict.cut);
    let whole = verdict
        .breaks
        .iter()
        .find(|b| b.from_start && b.at == "Job.Open")
        .expect("a path from a start ends at Job.Open");
    assert_eq!(whole.key, "Kick.Go > Job.Join > Job.Lend > Job.Open");
}

#[test]
fn the_same_rule_proves_once_the_last_move_puts_it_right() {
    let may_send = outside_sends(&["Add", "Go"]);
    let options = CrossOptions {
        facts: chain_facts(),
        may_send: Some(&may_send),
        ..CrossOptions::default()
    };
    let verdict = verdict(
        &specs(&[fixed(JOB, JOB_FIX), HOME.into(), KICK.into()]),
        &options,
    );
    assert!(verdict.proved, "{:?}", verdict.breaks);
    assert!(verdict.paths > 0);
}

#[test]
fn a_rule_nobody_can_reach_says_so() {
    let may_send = outside_sends(&[]);
    let options = CrossOptions {
        may_send: Some(&may_send),
        ..CrossOptions::default()
    };
    let verdict = verdict(&specs(&[POST.into(), THREAD.into()]), &options);
    assert!(verdict.proved, "{:?}", verdict.breaks);
    assert_eq!(verdict.paths, 0);
}

#[test]
fn a_named_record_never_made_reads_its_first_status() {
    let may_send = outside_sends(&["Add"]);
    let options = CrossOptions {
        may_send: Some(&may_send),
        ..CrossOptions::default()
    };
    let broken = verdict(&specs(&[POST.into(), THREAD.into()]), &options);
    assert!(!broken.proved);
    assert!(broken.breaks.iter().any(|b| b.at == "Post.Add"));

    let made = verdict(&specs(&[fixed(POST, POST_FIX), THREAD.into()]), &options);
    assert!(made.proved, "{:?}", made.breaks);
}

#[test]
fn two_rules_prove_together_what_neither_proves_alone() {
    let specs = specs(&[BOX.into(), LID.into()]);
    let may_send = outside_sends(&["Add", "Shut"]);
    let options = CrossOptions {
        may_send: Some(&may_send),
        ..CrossOptions::default()
    };
    let rule = cross_rules(&specs).remove(0);
    assert!(!check_cross_rule(&specs, &rule, &options).proved);
    assert!(verdict(&specs, &options).proved);
}

#[test]
fn a_reaction_on_its_way_can_still_break_a_rule() {
    let may_send = outside_sends(&["Add", "Take", "Clear"]);
    let options = CrossOptions {
        may_send: Some(&may_send),
        ..CrossOptions::default()
    };
    let broken = verdict(&specs(&[ALARM.into(), CASE.into()]), &options);
    assert!(!broken.proved);
    assert!(broken.breaks.iter().any(|b| b.at == "Case.Open"));

    let fixed = verdict(&specs(&[ALARM.into(), fixed(CASE, CASE_FIX)]), &options);
    assert!(fixed.proved, "{:?}", fixed.breaks);
}
