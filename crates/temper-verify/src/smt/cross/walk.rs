//! The search: each start, a move or a reaction arriving, followed through
//! everything it sets off until nothing is in flight, then the rule is asked.
//!
//! One path is one line of solver scopes, so a branch the path condition rules
//! out costs one query and is never walked.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use temper_spec::automaton::{Automaton, VarType};
use temper_spec::predicate::Expr;
use z3::ast::{Bool, Int};
use z3::{Params, SatResult, Solver};

use super::scope::{Scope, Send, Target, sends_of, stored_field, written_by_status};
use super::world::{ABSENT, Atoms, EMPTY, Reader, Rec, World};
use super::{CrossBreak, CrossFact, CrossRule};

mod deliver;
mod moves;
mod report;
mod run;

/// For each type, the fields that hold the ids of records of it that were made
/// by a send: the type of the holder, the field, and whether the kernel always
/// mints that id, as it does for a spawn that names none.
fn id_holders(specs: &BTreeMap<String, Automaton>) -> BTreeMap<String, Vec<IdHolder>> {
    let mut holders: BTreeMap<String, BTreeSet<IdHolder>> = BTreeMap::new();
    for (owner, spec) in specs {
        for action in &spec.actions {
            for send in sends_of(owner, action) {
                if let Some(field) = stored_field(&send) {
                    let minted = matches!(send.target, Target::Spawn { id: None, .. });
                    holders
                        .entry(send.to.to_string())
                        .or_default()
                        .insert(IdHolder {
                            owner: owner.clone(),
                            field: field.clone(),
                            minted,
                        });
                }
            }
        }
    }
    holders
        .into_iter()
        .map(|(ty, fields)| (ty, fields.into_iter().collect()))
        .collect()
}

/// A record that has just moved, and the reactions its move sends.
struct Firing<'s, 'b> {
    /// Its index in the world.
    source: usize,
    /// Its name in a counterexample.
    name: &'b str,
    /// The move's parameters.
    params: &'b BTreeMap<String, Int>,
    /// The reactions.
    sends: &'b [Send<'s>],
    /// The moves that led here.
    chain: &'b [(String, String)],
    /// The ids its spawns were given, in order.
    spawned: &'b [Int],
}

/// Moves, as type and name.
type Moves = BTreeSet<(String, String)>;

/// A field that holds the id of a record its owner made.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct IdHolder {
    /// The type of the record holding it.
    owner: String,
    /// The field.
    field: String,
    /// The kernel always mints this id, so nobody outside can pick it.
    minted: bool,
}

/// Most queries one rule's check may ask, so a rule that branches beyond this
/// reads as cut and never as proved.
const MOST_QUERIES: usize = 2_000_000;
/// How long Z3 may think about one query, in milliseconds.
const QUERY_MS: u32 = 5_000;
/// Reactions in flight beyond this go in the order they were sent: the orders
/// of more than this grow as a factorial.
const REORDER_UP_TO: usize = 3;

/// A reaction in flight.
#[derive(Clone)]
struct Flight {
    to: String,
    action: String,
    id: Int,
    params: BTreeMap<String, Int>,
    /// It always makes a new record.
    fresh: bool,
    chain: Vec<(String, String)>,
    from: String,
}

/// One taken step of a path, and the values to print with it.
#[derive(Clone)]
struct Step {
    text: String,
    key: String,
    shown: Vec<(String, Int)>,
}

/// The state of one rule's search.
pub(super) struct Walk<'a> {
    /// For each type, the fields that hold the ids made for it.
    makers: BTreeMap<String, Vec<IdHolder>>,
    scope: &'a Scope<'a>,
    owner: &'a str,
    rule: &'a Expr,
    atoms: Atoms,
    solver: Solver,
    written: BTreeMap<String, BTreeMap<String, BTreeSet<String>>>,
    /// The records before the step.
    before: World,
    /// The records a root starts on: one per type, since the rest are alike,
    /// and each fact, which is not.
    start_on: Vec<usize>,
    /// How many paths ended with nothing in flight.
    pub paths: usize,
    /// How many queries Z3 was asked.
    pub queries: usize,
    /// Whether a query went unanswered or the budget ran out.
    pub cut: bool,
    /// What it found, one per distinct path.
    pub breaks: BTreeMap<String, CrossBreak>,
    /// When to stop.
    deadline: Option<Instant>,
    /// The moves that can break the rule: a break is named after the last of
    /// them on its path.
    breakers: BTreeSet<(String, String)>,
    /// The rest of this start can tell nothing new.
    stop: bool,
    /// While following chains: the breaks still wanting a whole path.
    wanted: Option<BTreeSet<String>>,
    /// Rules about how records name each other, assumed of senders too.
    links: Vec<CrossRule>,
}

impl<'a> Walk<'a> {
    /// A search over `records` records of each type in scope, plus `facts`.
    pub fn new(
        scope: &'a Scope<'a>,
        owner: &'a str,
        rule: &'a Expr,
        records: usize,
        facts: &[CrossFact],
        deadline: Option<Instant>,
    ) -> Self {
        let solver = Solver::new();
        let mut params = Params::new();
        params.set_u32("timeout", QUERY_MS);
        solver.set_params(&params);
        let mut walk = Walk {
            scope,
            owner,
            rule,
            atoms: Atoms::default(),
            solver,
            written: BTreeMap::new(),
            before: World::default(),
            start_on: Vec::new(),
            paths: 0,
            queries: 0,
            cut: false,
            breaks: BTreeMap::new(),
            deadline,
            breakers: BTreeSet::new(),
            stop: false,
            wanted: None,
            links: Vec::new(),
            makers: id_holders(scope.specs),
        };
        walk.build(records, facts);
        walk
    }

    fn spec(&self, ty: &str) -> Option<&'a Automaton> {
        self.scope.specs.get(ty)
    }

    fn build(&mut self, records: usize, facts: &[CrossFact]) {
        let mut types = self.scope.types.clone();
        types.extend(self.scope.relevant.iter().map(|(ty, _)| ty.clone()));
        types.extend(facts.iter().map(|f| f.ty.clone()));
        let types: Vec<String> = types
            .into_iter()
            .filter(|ty| self.spec(ty).is_some())
            .collect();
        for ty in &types {
            if let Some(spec) = self.spec(ty) {
                self.written.insert(ty.clone(), written_by_status(spec));
            }
            for i in 1..=records {
                if i == 1 {
                    self.start_on.push(self.before.records.len());
                }
                let rec = self.fresh_record(ty, format!("{ty}#{i}"));
                self.before.records.push(rec);
            }
            let unmade = self.fresh_record(ty, format!("{ty} not made"));
            self.solver.assert(unmade.exists.not());
            self.before.unmade.insert(ty.clone(), unmade);
            for fact in facts.iter().filter(|f| &f.ty == ty) {
                let mut rec = self.fresh_record(ty, format!("{ty}'{}'", fact.id));
                rec.id = Int::from_i64(self.atoms.atom(&fact.id));
                let statuses: Vec<Bool> = fact
                    .statuses
                    .iter()
                    .map(|s| rec.status.eq(Int::from_i64(self.atoms.atom(s))))
                    .collect();
                self.solver.assert(Bool::or(&statuses));
                self.solver.assert(&rec.exists);
                self.start_on.push(self.before.records.len());
                self.before.records.push(rec);
            }
        }
        for ty in &types {
            let of = self.before.of(ty);
            for (n, a) in of.iter().enumerate() {
                for b in &of[n + 1..] {
                    let same = self.before.records[*a].id.eq(&self.before.records[*b].id);
                    self.solver.assert(same.not());
                }
            }
        }
    }

    /// From here on these hold of every record before the step: each record's
    /// own rules, this rule and `others`, the rules assumed with it. Only
    /// states with nothing in flight meet them, so asking which moves can flip
    /// a rule comes before this.
    pub fn assume_rules(&mut self, others: &[CrossRule]) {
        self.links = others.iter().filter(|rule| rule.link).cloned().collect();
        let before = self.before.clone();
        for rec in &before.records {
            let Some(spec) = self.spec(&rec.ty) else {
                continue;
            };
            let own: Vec<&Expr> = spec
                .invariants
                .iter()
                .filter(|inv| inv.assert.related_types().is_empty())
                .map(|inv| &inv.assert)
                .chain(
                    others
                        .iter()
                        .filter(|rule| rule.owner == rec.ty)
                        .map(|rule| &rule.assert),
                )
                .chain((rec.ty == self.owner).then_some(self.rule))
                .collect();
            for rule in own {
                let holds = self.read(rec, &before, None, rule);
                self.solver.assert(rec.exists.implies(holds));
            }
        }
    }

    /// Which moves of the scope can change the rule's truth with one step:
    /// those that can make it false, and those that can make it true.
    pub fn flips(&mut self) -> (Moves, Moves) {
        let mut breakers = BTreeSet::new();
        let mut fixers = BTreeSet::new();
        let before = self.before.clone();
        let owners = before.of(self.owner);
        let moves: Vec<(String, String)> = self.scope.relevant.iter().cloned().collect();
        for (ty, name) in moves {
            let Some(action) = self.scope.action(&ty, &name) else {
                continue;
            };
            let on: Vec<usize> = self
                .start_on
                .iter()
                .copied()
                .filter(|i| before.records[*i].ty == ty)
                .collect();
            for i in on {
                self.solver.push();
                let params = self.free_params(action);
                let rec = before.records[i].clone();
                let enabled = self.enabled(&rec, &before, action, &params);
                self.solver.assert(&enabled);
                let (next, _) = self.apply(&rec, action, &params);
                let mut after = before.clone();
                after.records[i] = next;
                let mut breaks = Vec::new();
                let mut fixes = Vec::new();
                for &o in &owners {
                    let (was, now) = (before.records[o].clone(), after.records[o].clone());
                    let held = self.read(&was, &before, None, self.rule);
                    let holds = self.read(&now, &after, None, self.rule);
                    let held = was.exists.implies(held);
                    let holds = now.exists.implies(holds);
                    breaks.push(Bool::and(&[held.clone(), holds.not()]));
                    fixes.push(Bool::and(&[held.not(), holds]));
                }
                for (set, wanted) in [(&mut breakers, breaks), (&mut fixers, fixes)] {
                    self.solver.push();
                    self.solver.assert(Bool::or(&wanted));
                    if self.sat() {
                        set.insert((ty.clone(), name.clone()));
                    }
                    self.solver.pop(1);
                }
                self.solver.pop(1);
            }
        }
        (breakers, fixers)
    }

    fn read(
        &mut self,
        rec: &Rec,
        world: &World,
        params: Option<&BTreeMap<String, Int>>,
        expr: &Expr,
    ) -> Bool {
        let reader = Reader {
            rec,
            world,
            params,
            spec: self.scope.specs.get(&rec.ty),
        };
        reader.truth(&mut self.atoms, expr)
    }

    /// A record with free values, within what its spec allows. One not made
    /// yet holds its starting values.
    fn fresh_record(&mut self, ty: &str, name: String) -> Rec {
        let exists = Bool::new_const(format!("{name}.exists"));
        let id = Int::new_const(format!("{name}.Id"));
        self.solver.assert(id.eq(Int::from_i64(EMPTY)).not());
        self.solver.assert(id.eq(Int::from_i64(ABSENT)).not());
        let status = Int::new_const(format!("{name}.status"));
        let spec = self.spec(ty);
        let names: Vec<String> = self
            .scope
            .fields
            .get(ty)
            .map(|f| f.iter().cloned().collect())
            .unwrap_or_default();
        let fields: BTreeMap<String, Int> = names
            .iter()
            .map(|field| (field.clone(), Int::new_const(format!("{name}.{field}"))))
            .collect();
        let rec = Rec {
            name,
            ty: ty.to_string(),
            id,
            exists,
            status,
            fields,
        };
        if let Some(spec) = spec {
            self.limit_status(&rec, spec);
            let started = rec
                .status
                .eq(Int::from_i64(self.atoms.atom(&spec.automaton.initial)));
            self.solver.assert(rec.exists.not().implies(&started));
            self.limit_fields(&rec, spec);
        }
        rec
    }

    fn limit_status(&mut self, rec: &Rec, spec: &Automaton) {
        let states: Vec<Bool> = spec
            .automaton
            .states
            .iter()
            .map(|s| rec.status.eq(Int::from_i64(self.atoms.atom(s))))
            .collect();
        self.solver.assert(Bool::or(&states));
    }

    /// A field's kind bounds it, and a status its moves on the way there
    /// never write leaves it at its starting value.
    fn limit_fields(&mut self, rec: &Rec, spec: &Automaton) {
        for (field, value) in &rec.fields {
            let Some(var) = spec.state.iter().find(|v| &v.name == field) else {
                continue;
            };
            // A record stores a value for every field from the moment it is
            // made: only a param can be absent.
            self.solver.assert(value.eq(Int::from_i64(ABSENT)).not());
            let zero = Int::from_i64(0);
            match var.var_type {
                VarType::Counter | VarType::List => self.solver.assert(value.ge(&zero)),
                VarType::Bool => self
                    .solver
                    .assert(Bool::or(&[value.eq(&zero), value.eq(Int::from_i64(1))])),
                VarType::String | VarType::Int => {}
            }
            let initial = Int::from_i64(self.atoms.initial(&var.initial));
            self.solver
                .assert(rec.exists.not().implies(value.eq(&initial)));
            let Some(by_status) = self.written.get(&rec.ty).cloned() else {
                continue;
            };
            for (status, written) in &by_status {
                if !written.contains(field) {
                    let here = rec.status.eq(Int::from_i64(self.atoms.atom(status)));
                    self.solver
                        .assert(Bool::and(&[rec.exists.clone(), here]).implies(value.eq(&initial)));
                }
            }
        }
    }

    fn sat(&mut self) -> bool {
        self.queries += 1;
        if self.queries > MOST_QUERIES || self.deadline.is_some_and(|d| Instant::now() > d) {
            self.cut = true;
            return false;
        }
        match self.solver.check() {
            SatResult::Unsat => false,
            SatResult::Sat => true,
            SatResult::Unknown => {
                self.cut = true;
                false
            }
        }
    }
}
