//! The kinds of outside sender a tenant's policies tell apart.
//!
//! A question about "someone outside" leaves the principal unknown, so a
//! walk that asks it at each step of a chain can let one step through for
//! one principal and refuse the next for another, which no real sender does:
//! a chain runs as one principal throughout. [`SendAnalysis::outside_kinds`]
//! splits the outside senders by what the policies test of the principal
//! (its type, its attributes, the context keys only the kernel sets), only
//! as far as a split changes a residual policy, and joins kinds whose
//! answers are definite and alike. Every outside principal is of
//! one kind, so asking each question for each kind misses no sender.

use std::collections::{BTreeMap, BTreeSet};

use cedar_policy::Effect;
use serde_json::Value as Json;

use super::json::{entity_parts, single};
use super::profile::{Fact, Kind, Profile, UNKNOWN};
use super::request::{Request, Results, TRUE};
use super::{Asker, Cause, May, SendAnalysis, decide};
use crate::platform_keys;

mod memory;
mod split;
use memory::Memory;

/// A question a walk asks of each outside sender: whether it may send
/// `action` to a `resource_type` record, for a move with `cause`, whatever
/// the record holds.
#[derive(Debug, Clone)]
pub struct Question<'a> {
    pub action: &'a str,
    pub resource_type: &'a str,
    pub cause: Cause,
}

/// A kind of outside sender being told apart: what it is known to be, each
/// question's answer for it, and the tests some open answer turns on.
struct Node {
    profile: Profile,
    answers: Vec<May>,
    undecided: BTreeSet<Test>,
}

/// Where a sender's value is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Place {
    Principal,
    Context,
}

/// One thing about a sender a policy tests.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Test {
    /// The principal's entity type.
    Kind,
    /// A principal attribute, or a context key only the kernel sets.
    Named(Place, String),
    /// Whether that value matches a `like` pattern, by the pattern's JSON.
    Like(Place, String, String),
}

impl Test {
    /// Whether `profile` holds nothing or some other value here.
    fn any(&self, profile: &Profile) -> bool {
        let fact = match self {
            Test::Kind => profile.kind.as_ref(),
            Test::Named(place, name) => facts(profile, *place).get(name),
            Test::Like(..) => None,
        };
        matches!(fact, Some(Fact::Absent | Fact::Other { .. }))
    }

    /// What it reads, as a profile names it.
    fn name(&self) -> String {
        match self {
            Test::Kind => "kind".to_string(),
            Test::Named(Place::Principal, name) | Test::Like(Place::Principal, name, _) => {
                name.clone()
            }
            Test::Named(Place::Context, name) | Test::Like(Place::Context, name, _) => {
                format!("context.{name}")
            }
        }
    }
}

/// What the policies test of a sender.
#[derive(Default)]
struct Tests {
    /// Each policy's tests, in policy order.
    reads: Vec<BTreeSet<Test>>,
    /// The tests in each policy's principal scope.
    scopes: Vec<BTreeSet<Test>>,
    /// The entity types the policies compare the principal with.
    kinds: Vec<String>,
    /// The literals the policies compare each value with.
    literals: BTreeMap<(Place, String), Vec<Json>>,
}

impl SendAnalysis {
    /// The kinds of outside sender these policies tell apart on
    /// `questions`: together they are every outside principal, and every
    /// question has one answer for all of a kind's. `reserve` accounts for
    /// the live construction's bytes before growth; denial aborts the
    /// partition rather than returning an incomplete set of senders.
    pub fn outside_kinds(
        &self,
        questions: &[Question<'_>],
        reserve: &dyn Fn(usize) -> Result<(), String>,
    ) -> Result<Vec<Kind>, String> {
        reserve(memory::tests_room(&self.policies, questions.len()))?;
        let tests = Tests::read(&self.policies);
        let none = BTreeMap::new();
        let anyone = Asker::Outside;
        // Knowing more of the sender only rules policies out, so a policy
        // that cannot apply to someone unknown applies to no kind.
        let candidates: Vec<Vec<usize>> = questions
            .iter()
            .map(|question| {
                let request = ask(&anyone, &UNKNOWN, question, &none);
                (0..self.policies.len())
                    .filter(|&at| request.applies(&self.policies[at].1).can(TRUE))
                    .collect()
            })
            .collect();
        let memory = Memory::new(&tests, &self.policies, &candidates, reserve);
        memory.reserve(4)?;
        let read = |profile: Profile| {
            let mut answers = Vec::with_capacity(questions.len());
            let mut undecided = BTreeSet::new();
            for (question, candidates) in questions.iter().zip(&candidates) {
                let request = ask(&anyone, &profile, question, &none);
                let applies: Vec<(usize, Results)> = candidates
                    .iter()
                    .map(|&at| (at, request.applies(&self.policies[at].1)))
                    .collect();
                let answer = decide(applies.iter().map(|(at, r)| (self.policies[*at].0, *r)));
                if answer == May::Maybe {
                    for (at, applies) in &applies {
                        if applies.can(TRUE) && *applies != TRUE {
                            undecided.extend(tests.reads[*at].iter().cloned());
                        }
                    }
                }
                answers.push(answer);
            }
            undecided.retain(|test| tests.open(&profile, test));
            Node {
                profile,
                answers,
                undecided,
            }
        };
        // What every policy that reads `test` leaves open for `node`'s
        // senders. Senders that differ only in `test` and leave these the
        // same answer alike from here on, however the rest comes to be
        // known, so one of them stands for the others. None when a part
        // still open reads `test`.
        let signature = |node: &Node, test: &Test| -> Option<Vec<String>> {
            let mut parts = Vec::new();
            for (question, candidates) in questions.iter().zip(&candidates) {
                let request = ask(&anyone, &node.profile, question, &none);
                for &at in candidates
                    .iter()
                    .filter(|&&at| tests.touch(&tests.reads[at], test))
                {
                    let policy = &self.policies[at].1;
                    let applies = request.applies(policy);
                    let decided = applies == TRUE || !applies.can(TRUE);
                    if !decided && tests.touch(&tests.scopes[at], test) {
                        return None;
                    }
                    let reads = |expr: &Json| tests.touch(&Tests::of(expr), test);
                    parts.push(request.residual(policy, &reads)?.to_string());
                }
            }
            Some(parts)
        };
        // Split a kind on the first test it leaves open that some answer
        // turns on; a test no answer turns on here tells nothing apart.
        let mut leaves: Vec<Node> = Vec::new();
        let mut open = vec![read(Profile::default())];
        while let Some(node) = open.pop() {
            // Include vector spare capacity, reallocation overlap, children,
            // signatures, and the result being assembled before allocating.
            memory.reserve(4 * (open.len() + leaves.len() + memory.branches + 1))?;
            let split = node.undecided.first().map(|test| {
                let children: Vec<Node> = tests
                    .split(&node.profile, test)
                    .into_iter()
                    .map(read)
                    .collect();
                // Equal Maybe answers can hide a condition on another test.
                // Only equal residuals let one branch stand for another.
                let mut alike: Vec<(Vec<String>, Node)> = Vec::new();
                let mut apart = Vec::new();
                for child in children {
                    match signature(&child, test) {
                        Some(sig) => match alike.iter_mut().find(|(known, _)| *known == sig) {
                            // An example that stands for any value says
                            // nothing of the kind.
                            Some((_, first)) if test.any(&child.profile) => {
                                first.profile.stand_ins.insert(test.name());
                            }
                            Some(_) => {}
                            None => alike.push((sig, child)),
                        },
                        None => apart.push(child),
                    }
                }
                apart.extend(alike.into_iter().map(|(_, child)| child));
                apart
            });
            match split {
                Some(children) => open.extend(children),
                None => leaves.push(node),
            }
        }
        let mut kinds: Vec<(Vec<May>, Vec<Profile>)> = Vec::new();
        for Node {
            answers, profile, ..
        } in leaves
        {
            // A Maybe may resolve differently once a rule or record is known.
            match kinds
                .iter_mut()
                .find(|(known, _)| !answers.contains(&May::Maybe) && *known == answers)
            {
                Some((_, kind)) => kind.push(profile),
                None => kinds.push((answers, vec![profile])),
            }
        }
        Ok(kinds
            .into_iter()
            .map(|(_, profiles)| Kind(profiles))
            .collect())
    }
}

fn ask<'a>(
    asker: &'a Asker,
    profile: &'a Profile,
    question: &'a Question<'_>,
    record: &'a BTreeMap<String, super::Attr>,
) -> Request<'a> {
    Request {
        asker,
        profile,
        action: question.action,
        resource_type: question.resource_type,
        record,
        cause: Some(&question.cause),
    }
}

/// The context keys only the kernel sets from the sender's security
/// context, which a reaction inherits with it. The cause's keys are the
/// question's own; a context approximated from bare ids marks itself.
fn sender_key(key: &str) -> bool {
    platform_keys::ALL.contains(&key)
        && !platform_keys::CAUSE.contains(&key)
        && key != platform_keys::TRIGGER_INHERITED_CONTEXT_APPROXIMATE
}

/// Where `expr` reads a sender's value from, when it is `principal` or
/// `context`.
fn place(expr: &Json) -> Option<Place> {
    match single(expr)? {
        ("Var", Json::String(var)) if var == "principal" => Some(Place::Principal),
        ("Var", Json::String(var)) if var == "context" => Some(Place::Context),
        _ => None,
    }
}

/// The sender's value `expr` reads, `principal.<a>` or a kernel's
/// `context.<k>`.
fn named(op: &str, arg: &Json) -> Option<(Place, String)> {
    if op != "." && op != "has" {
        return None;
    }
    let place = place(&arg["left"])?;
    let name = arg["attr"].as_str()?;
    (place == Place::Principal || sender_key(name)).then(|| (place, name.to_string()))
}

fn value(expr: &Json) -> Option<&Json> {
    match single(expr)? {
        ("Value", value) => Some(value),
        _ => None,
    }
}

impl Tests {
    fn read(policies: &[(Effect, Json)]) -> Self {
        let mut tests = Tests::default();
        for (_, policy) in policies {
            let mut reads = BTreeSet::new();
            let scope = &policy["principal"];
            match scope["op"].as_str() {
                Some("==") => tests.principal_is_entity(&scope["entity"], &mut reads),
                Some("is") => tests.principal_is_kind(scope["entity_type"].as_str(), &mut reads),
                _ => {}
            }
            tests.scopes.push(reads.clone());
            tests.walk(&policy["conditions"], &mut reads);
            tests.reads.push(reads);
        }
        tests.kinds.sort();
        tests.kinds.dedup();
        tests
    }

    /// The tests `expr` makes.
    fn of(expr: &Json) -> BTreeSet<Test> {
        let mut reads = BTreeSet::new();
        Tests::default().walk(expr, &mut reads);
        reads
    }

    /// Whether a part that makes `reads` turns on `test`.
    fn touch(&self, reads: &BTreeSet<Test>, test: &Test) -> bool {
        match test {
            Test::Named(place, name) => reads.iter().any(|read| match read {
                Test::Named(p, n) | Test::Like(p, n, _) => p == place && n == name,
                Test::Kind => false,
            }),
            _ => reads.contains(test),
        }
    }

    fn principal_is_kind(&mut self, kind: Option<&str>, reads: &mut BTreeSet<Test>) {
        // Someone outside is never the kernel's `System`.
        if let Some(kind) = kind.filter(|kind| *kind != "System") {
            self.kinds.push(kind.to_string());
            reads.insert(Test::Kind);
        }
    }

    fn principal_is_entity(&mut self, entity: &Json, reads: &mut BTreeSet<Test>) {
        let Some((kind, id)) = entity_parts(entity) else {
            return;
        };
        self.principal_is_kind(Some(kind), reads);
        let id_attr = (Place::Principal, "id".to_string());
        self.literal(&id_attr, Json::from(id));
        reads.insert(Test::Named(id_attr.0, id_attr.1));
    }

    fn literal(&mut self, of: &(Place, String), value: Json) {
        let literals = self.literals.entry(of.clone()).or_default();
        if !literals.contains(&value) {
            literals.push(value);
        }
    }

    fn walk(&mut self, expr: &Json, reads: &mut BTreeSet<Test>) {
        let Some((op, arg)) = single(expr) else {
            match expr {
                Json::Object(object) => object.values().for_each(|child| self.walk(child, reads)),
                Json::Array(items) => items.iter().for_each(|child| self.walk(child, reads)),
                _ => {}
            }
            return;
        };
        if op == "Value" {
            return;
        }
        if let Some(of) = named(op, arg) {
            reads.insert(Test::Named(of.0, of.1));
        }
        let (left, right) = (&arg["left"], &arg["right"]);
        let side = |expr: &Json| single(expr).and_then(|(op, arg)| named(op, arg));
        match op {
            "==" | "!=" => {
                for (subject, other) in [(left, right), (right, left)] {
                    let Some(other) = value(other) else { continue };
                    if let Some(of) = side(subject) {
                        self.literal(&of, other.clone());
                    } else if place(subject) == Some(Place::Principal) {
                        self.principal_is_entity(other, reads);
                    }
                }
            }
            "in" => {
                if let (Some(of), Some(("Set", Json::Array(members)))) = (side(left), single(right))
                {
                    for member in members.iter().filter_map(value) {
                        self.literal(&of, member.clone());
                    }
                }
            }
            "is" if place(left) == Some(Place::Principal) => {
                self.principal_is_kind(arg["entity_type"].as_str(), reads);
            }
            "like" => {
                if let Some(of) = side(left) {
                    reads.insert(Test::Like(of.0, of.1, arg["pattern"].to_string()));
                }
            }
            _ => {}
        }
        self.walk(arg, reads);
    }

    /// Whether `profile` leaves `test` open.
    fn open(&self, profile: &Profile, test: &Test) -> bool {
        match test {
            Test::Kind => profile.kind.is_none(),
            Test::Named(place, name) => !facts(profile, *place).contains_key(name),
            Test::Like(place, name, pattern) => matches!(
                facts(profile, *place).get(name),
                Some(Fact::Other { like, string, .. })
                    if *string != Some(false) && !like.contains_key(pattern)
            ),
        }
    }
}

fn facts(profile: &Profile, place: Place) -> &BTreeMap<String, Fact> {
    match place {
        Place::Principal => &profile.attrs,
        Place::Context => &profile.context,
    }
}
