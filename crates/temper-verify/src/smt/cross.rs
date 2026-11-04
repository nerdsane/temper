//! Rules over two records, checked by induction with Z3.
//!
//! Level 0 reads one record at a time. This reads a rule such as
//! `status != 'Unmade' => Hub[hub].status in ['Active']` over a few symbolic
//! records of each type it names, all values free:
//!
//! 1. Assume the rule holds of every record, and nothing is in flight.
//! 2. Ask which moves can make the rule false with one step. Any break needs
//!    one of them, which keeps the search small.
//! 3. Take one of them: sent by someone or the kernel, or arriving as a
//!    reaction from a sender that moved some time ago, its values free. Follow
//!    every reaction it sets off that can matter, in any order, until nothing
//!    is in flight. Each branch a guard or a refusal opens is one path.
//! 4. Ask Z3 whether the rule can now be false.
//!
//! No path can break it: the rule is inductive, so it holds in every state
//! with nothing in flight, for these records. A path can: Z3's model is a
//! counterexample to induction, whose starting state may be one no run
//! reaches, so it is a lead to confirm and not yet a bug.
//!
//! A reaction arriving at any time from a sender with free values covers the
//! orders a real run can deliver it in, at the cost of states no run reaches.
//! It reads as the kernel does: an empty reference reads null, any other id
//! reads its record, and one never made reads its first status. A reaction
//! through a field can make the record it names.
//!
//! What the check does not see: Cedar on a reaction, what a list holds, a call
//! in flight, more records of one type than the world holds, and two starts
//! overlapping except as one reaction from a free sender arriving late. The
//! rules hold between steps only, so a state in the middle of another start's
//! reactions is not a state it starts from. It also leaves out someone outside
//! making a record under an id a spawn has just minted, before the spawn lands.
//!
//! A free sender is not asked whether anyone can start it, so a path from one
//! is a lead. A verdict of proved says nothing about a rule's wording: a proof
//! with no paths means nothing can send a move that breaks it, which is what a
//! dead rule reads as too.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use temper_spec::automaton::Automaton;
use temper_spec::predicate::{Arg, Expr, parse};

mod scope;
mod walk;
mod world;

use scope::Scope;
use walk::Walk;

/// A rule that reads another record.
#[derive(Debug, Clone)]
pub struct CrossRule {
    /// The type it is declared on.
    pub owner: String,
    /// Its name.
    pub name: String,
    /// What must hold.
    pub assert: Expr,
    /// It says only how two records name each other once the named one is
    /// made, which no move in flight makes false for a moment, so a sender is
    /// assumed to meet it too.
    pub link: bool,
}

/// A record the app relies on existing, as data: `id` of `ty` is in one of
/// `statuses` before the step.
#[derive(Debug, Clone)]
pub struct CrossFact {
    /// The record's type.
    pub ty: String,
    /// Its id.
    pub id: String,
    /// The statuses it may be in.
    pub statuses: Vec<String>,
}

/// Whether someone outside may send a type's move as a request.
pub type MaySend<'a> = &'a dyn Fn(&str, &str) -> bool;

/// How to run a check.
#[derive(Default)]
pub struct CrossOptions<'a> {
    /// How many records of each type the world holds; 2 when 0.
    pub records: usize,
    /// The records assumed to exist.
    pub facts: Vec<CrossFact>,
    /// Whether someone outside may send a move as a request. Without it, every
    /// input may be sent.
    pub may_send: Option<MaySend<'a>>,
    /// How long one rule may take.
    pub limit: Option<Duration>,
}

/// One counterexample to induction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrossBreak {
    /// The last move of the path that can break the rule: what to look at.
    pub at: String,
    /// The moves of the path, in order. `~` marks a sender with free values.
    pub key: String,
    /// The path starts at a move someone or the kernel makes, so every step
    /// after it is one the specs send.
    pub from_start: bool,
    /// The record that breaks the rule.
    pub record: String,
    /// The path, with the values Z3 chose.
    pub steps: Vec<String>,
    /// The records the rule reads, before and after.
    pub records: Vec<String>,
}

/// What the check says of one rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrossVerdict {
    /// `Owner.Name`.
    pub rule: String,
    /// A candidate from [`link_rules`], not a rule of the app.
    pub link: bool,
    /// No path breaks it and nothing was cut.
    pub proved: bool,
    /// A query went unanswered or the limit was reached, so no path found is
    /// not a proof.
    pub cut: bool,
    /// Time ran out while following breaks back to where they start. It does
    /// not change the verdict.
    pub chains_cut: bool,
    /// The moves that can make the rule false with one step.
    pub breakers: Vec<String>,
    /// The moves that can make it true with one step.
    pub fixers: Vec<String>,
    /// Those moves and all that send them.
    pub moves: usize,
    /// Paths that ended with nothing in flight.
    pub paths: usize,
    /// Queries asked of Z3.
    pub queries: usize,
    /// Seconds taken.
    pub secs: f64,
    /// The counterexamples: for each move that breaks it, one with a free
    /// sender and one from a start, when there are.
    pub breaks: Vec<CrossBreak>,
}

/// Every rule in `specs` that reads another record.
pub fn cross_rules(specs: &BTreeMap<String, Automaton>) -> Vec<CrossRule> {
    specs
        .iter()
        .flat_map(|(owner, spec)| {
            spec.invariants
                .iter()
                .filter(|inv| !inv.assert.related_types().is_empty())
                .map(|inv| CrossRule {
                    owner: owner.clone(),
                    name: inv.name.clone(),
                    assert: inv.assert.clone(),
                    link: false,
                })
        })
        .collect()
}

/// A candidate rule for each field that a move fills with the id of a record
/// it makes and hands its own id to: once made, the record that field names
/// names this one back. Before it is made it names no one, and a reaction may
/// still be on its way to make it, so the rule says nothing then. Without these,
/// induction starts from records that disagree about who belongs to whom. They
/// are only candidates: [`check_cross_rules`] takes out those that do not hold.
pub fn link_rules(specs: &BTreeMap<String, Automaton>) -> Vec<CrossRule> {
    let mut rules: BTreeMap<String, CrossRule> = BTreeMap::new();
    for (owner, spec) in specs {
        for action in &spec.actions {
            for send in scope::sends_of(owner, action) {
                let Some(field) = scope::stored_field(&send) else {
                    continue;
                };
                let Some(target) = specs.get(send.to) else {
                    continue;
                };
                let first = &target.automaton.initial;
                for (back, arg) in &send.args {
                    if !matches!(arg, Arg::Var(var) if matches!(var.as_str(), "Id" | "id")) {
                        continue;
                    }
                    let to = send.to;
                    let text = format!(
                        "!empty({field}) && {to}[{field}].status != '{first}' => {to}[{field}].{back} == Id"
                    );
                    let name = format!("link {field} {}.{back}", send.to);
                    if let Ok(assert) = parse(&text) {
                        rules.entry(format!("{owner} {name}")).or_insert(CrossRule {
                            owner: owner.clone(),
                            name,
                            assert,
                            link: true,
                        });
                    }
                }
            }
        }
    }
    rules.into_values().collect()
}

/// Check every rule together: each is assumed while the others are checked,
/// the ones that do not hold are taken out, and it goes round again until what
/// is left holds. What is left is proved as a set. A rule that is not is shown
/// with what breaks it given only what is proved, its leads the most honest.
pub fn check_cross_rules(
    specs: &BTreeMap<String, Automaton>,
    rules: &[CrossRule],
    options: &CrossOptions<'_>,
) -> Vec<CrossVerdict> {
    let mut proved: Vec<CrossRule> = rules.to_vec();
    loop {
        let kept: Vec<CrossRule> = proved
            .iter()
            .filter(|rule| check(specs, rule, &proved, options, false).proved)
            .cloned()
            .collect();
        let same = kept.len() == proved.len();
        proved = kept;
        if same {
            break;
        }
    }
    rules
        .iter()
        .map(|rule| check(specs, rule, &proved, options, true))
        .collect()
}

/// Check one rule by induction, assuming only what the specs declare of one
/// record.
pub fn check_cross_rule(
    specs: &BTreeMap<String, Automaton>,
    rule: &CrossRule,
    options: &CrossOptions<'_>,
) -> CrossVerdict {
    check(specs, rule, &[], options, true)
}

fn check(
    specs: &BTreeMap<String, Automaton>,
    rule: &CrossRule,
    assumed: &[CrossRule],
    options: &CrossOptions<'_>,
    chains: bool,
) -> CrossVerdict {
    let start = Instant::now();
    let deadline = options.limit.map(|limit| start + limit);
    let records = if options.records == 0 {
        2
    } else {
        options.records
    };

    // Which moves can change the rule's truth at all, one step each.
    let broad = Scope::broad(specs, &rule.owner, &rule.assert);
    let mut flipping = Walk::new(
        &broad,
        &rule.owner,
        &rule.assert,
        records,
        &options.facts,
        deadline,
    );
    let (breakers, fixers) = flipping.flips();
    let mut queries = flipping.queries;
    let flips_cut = flipping.cut;

    // A break needs one of those to be the step. What it sets off can still
    // put the rule right, so the moves that can are followed too.
    let mut seeds = breakers.clone();
    seeds.extend(fixers.iter().cloned());
    let scope = Scope::narrowed(
        specs,
        &rule.owner,
        &rule.assert,
        &seeds,
        options.may_send,
        assumed,
    );
    let mut walk = Walk::new(
        &scope,
        &rule.owner,
        &rule.assert,
        records,
        &options.facts,
        deadline,
    );
    walk.assume_rules(assumed);
    walk.check_breakers(&breakers);
    let cut = flips_cut || walk.cut;
    let proved = walk.breaks.is_empty() && !cut;

    // A break found so: how far back does its path go? Only for what is left
    // of the time, since the answer no longer changes the verdict.
    let mut chains_cut = false;
    if chains && !walk.breaks.is_empty() {
        walk.until(deadline);
        walk.check_chains(&breakers);
        chains_cut = walk.cut;
    }
    queries += walk.queries;
    let shown = |set: &BTreeSet<(String, String)>| -> Vec<String> {
        set.iter()
            .map(|(ty, name)| format!("{ty}.{name}"))
            .collect()
    };
    let breaks: Vec<CrossBreak> = walk.breaks.into_values().collect();
    CrossVerdict {
        rule: format!("{}.{}", rule.owner, rule.name),
        link: rule.link,
        proved,
        cut,
        chains_cut,
        breakers: shown(&breakers),
        fixers: shown(&fixers),
        moves: scope.relevant.len(),
        paths: walk.paths,
        queries,
        secs: start.elapsed().as_secs_f64(),
        breaks,
    }
}
