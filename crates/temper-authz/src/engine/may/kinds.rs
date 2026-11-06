//! The kinds of outside sender a tenant's policies tell apart.
//!
//! A question about "someone outside" leaves the principal unknown, so a
//! walk that asks it at each step of a chain can let one step through for
//! one principal and refuse the next for another, which no real sender does:
//! a chain runs as one principal throughout. [`SendAnalysis::outside_kinds`]
//! splits the outside senders by what the policies test of the principal
//! (its type, its attributes, the context keys only the kernel sets), only
//! as far as a split changes a residual policy, keeps one sender of those
//! the policies leave the same, and joins kinds whose answers are definite
//! and alike. Every outside principal is of one kind, so asking each
//! question for each kind misses no sender.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use cedar_policy::Effect;
use serde_json::Value as Json;

use super::json::{entity_parts, single};
use super::profile::{Asked, Fact, Kind, Profile, UNKNOWN};
use super::request::{Request, TRUE};
use super::{Asker, Cause, May, SendAnalysis, decide};
use crate::platform_keys;

mod alike;
mod forms;
mod memory;
mod split;
mod tests_of;
use alike::Alike;
use forms::{Classes, Forms, Part, reads_of, reads_whole_sender};
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

/// A kind of outside sender and its answer to each question it was told
/// apart on, in their order: what [`SendAnalysis::may`] answers for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answered {
    pub kind: Kind,
    pub answers: Vec<May>,
}

impl Answered {
    /// The bytes it retains, its kind's and its answers'.
    pub fn bytes(&self) -> usize {
        self.kind.bytes() + self.answers.capacity() * size_of::<May>()
    }
}

/// A kind of outside sender being told apart: what it is known to be, each
/// question's answer for it, and the tests some open answer turns on.
struct Node {
    profile: Profile,
    answers: Vec<May>,
    undecided: BTreeSet<Test>,
    /// What decides this kind's senders from here on ([`Forms`]).
    form: Vec<u32>,
    /// What each class of policy says of this kind ([`Part`]).
    known: Vec<Option<Part>>,
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
    /// The entity types and literals of the policies some question may
    /// apply: no other tells senders apart, so a split need not try them.
    asked_kinds: Vec<String>,
    asked_literals: BTreeMap<(Place, String), Vec<Json>>,
}

impl SendAnalysis {
    /// The kinds of outside sender these policies tell apart on
    /// `questions`, each with its answers to them: together they are every
    /// outside principal, and every question has one answer for all of a
    /// kind's. `reserve` accounts for
    /// the live construction's bytes before growth; denial aborts the
    /// partition rather than returning an incomplete set of senders.
    pub fn outside_kinds(
        &self,
        questions: &[Question<'_>],
        reserve: &dyn Fn(usize) -> Result<(), String>,
    ) -> Result<Vec<Answered>, String> {
        let key = format!("{questions:?}");
        let found = |kinds: &HashMap<String, Vec<Answered>>| kinds.get(&key).cloned();
        let held = self
            .kinds
            .lock()
            .map_err(|_| "a partition panicked".to_string())?;
        if let Some(kinds) = found(&held) {
            reserve(kinds.iter().map(Answered::bytes).sum())?;
            return Ok(kinds);
        }
        drop(held);
        let kinds = self.partition(questions, reserve)?;
        self.note_someone(questions, &kinds);
        let mut held = self
            .kinds
            .lock()
            .map_err(|_| "a partition panicked".to_string())?;
        held.insert(key, kinds.clone());
        Ok(kinds)
    }

    pub(super) fn partition(
        &self,
        questions: &[Question<'_>],
        reserve: &dyn Fn(usize) -> Result<(), String>,
    ) -> Result<Vec<Answered>, String> {
        reserve(memory::tests_room(&self.policies, questions.len()))?;
        let asked = Arc::new(asked(questions));
        let mut tests = Tests::read(&self.policies);
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
        tests.ask(&self.policies, &candidates);
        // A question someone unknown has a definite answer to has it for
        // every kind: it adds the same answer to every node and reads
        // nothing, so it never splits one, and the nodes ask only the rest.
        let unknown: Vec<May> = questions
            .iter()
            .zip(&candidates)
            .map(|(question, candidates)| {
                let request = ask(&anyone, &UNKNOWN, question, &none);
                decide(
                    candidates
                        .iter()
                        .map(|&at| (self.policies[at].0, request.applies(&self.policies[at].1))),
                )
            })
            .collect();
        let maybe: Vec<usize> = (0..questions.len())
            .filter(|&at| unknown[at] == May::Maybe)
            .collect();
        let questions: Vec<Question<'_>> = maybe.iter().map(|&at| questions[at].clone()).collect();
        let questions = &questions[..];
        let candidates: Vec<Vec<usize>> = maybe.iter().map(|&at| candidates[at].clone()).collect();
        let memory = Memory::new(&tests, &self.policies, &candidates, reserve);
        memory.reserve(4, 0)?;
        let forms = Forms::default();
        let mut classes = Classes::read(&self.policies, questions, &candidates);
        // Questions that leave every candidate in the same class answer
        // alike for every kind and leave the same parts open: one of them
        // stands for the rest, and its answer is theirs.
        let mut held = HashMap::new();
        let mut stands = Vec::with_capacity(classes.of.len());
        let mut answered_by = Vec::with_capacity(classes.of.len());
        for of in &classes.of {
            let count = held.len();
            let by = *held.entry(of.clone()).or_insert(count);
            stands.push(by == count);
            answered_by.push(by);
        }
        let kept = |at: &usize| stands[*at];
        let questions: Vec<Question<'_>> = (0..questions.len())
            .filter(kept)
            .map(|at| questions[at].clone())
            .collect();
        let questions = &questions[..];
        let candidates: Vec<Vec<usize>> = (0..candidates.len())
            .filter(kept)
            .map(|at| candidates[at].clone())
            .collect();
        classes.of = (0..classes.of.len())
            .filter(kept)
            .map(|at| classes.of[at].clone())
            .collect();
        let read = |profile: Profile, parent: Option<&Node>| {
            let mut answers = Vec::with_capacity(questions.len());
            let mut form = Vec::with_capacity(questions.len() + 1);
            // What the parts still open read: the facts they will go on to read.
            let mut reads = BTreeSet::new();
            let mut whole = false;
            // A policy reads alike for every question of its class, and what
            // it has decided for a kind it decides for all of that kind.
            let mut known: Vec<Option<Part>> = parent.map_or_else(
                || vec![None; classes.count],
                |parent| parent.known.iter().map(Part::stays).collect(),
            );
            for (at_question, question) in questions.iter().enumerate() {
                let request = ask(&anyone, &profile, question, &none);
                let of = &classes.of[at_question];
                let mut decided = Vec::with_capacity(of.len());
                for (&at, &class) in candidates[at_question].iter().zip(of) {
                    let part = known[class].get_or_insert_with(|| Part {
                        applies: request.applies(&self.policies[at].1),
                        text: None,
                    });
                    decided.push((self.policies[at].0, part.applies));
                }
                let answer = decide(decided.into_iter());
                answers.push(answer);
                if answer != May::Maybe {
                    // More known of the sender cannot change a definite answer.
                    form.push(forms.answer(answer));
                    continue;
                }
                let mut left = Vec::new();
                for (&at, &class) in candidates[at_question].iter().zip(of) {
                    let Some(part) = known[class].as_mut().filter(|part| part.applies.can(TRUE))
                    else {
                        continue;
                    };
                    let text = *part.text.get_or_insert_with(|| {
                        let (effect, policy) = &self.policies[at];
                        let note = |expr: &Json| {
                            reads.extend(Tests::of(expr));
                            whole |= reads_whole_sender(expr);
                            false
                        };
                        let note = std::cell::RefCell::new(note);
                        let residual = request.residual(policy, &|expr| (note.borrow_mut())(expr));
                        reads.extend(tests.scopes[at].iter().cloned());
                        forms.text(&format!("{effect:?}{}", residual.unwrap_or(Json::Null)))
                    });
                    left.push(text);
                }
                left.sort_unstable();
                left.dedup();
                form.push(forms.list(left));
            }
            form.push(forms.text(&reads_of(&profile, &reads, whole)));
            let undecided = reads
                .into_iter()
                .filter(|test| tests.open(&profile, test))
                .collect();
            Node {
                profile,
                answers,
                undecided,
                form,
                known,
            }
        };
        // Split a kind on the first test some part still open reads. Senders
        // with one form answer alike from here on, however the rest comes to
        // be known, so the first stands for the others: without this, the
        // tests no policy reads together multiply the kinds out. What the
        // others hold is kept ([`Alike`]), so a kind still describes them
        // when asked what its split did not ask.
        let mut leaves: Vec<(usize, Node)> = Vec::new();
        let mut seen = HashMap::new();
        let first = read(Profile::default(), None);
        let mut alike = Alike::new();
        seen.insert(first.form.clone(), Alike::ROOT);
        let mut open = vec![(Alike::ROOT, first)];
        while let Some((at, node)) = open.pop() {
            // Include vector spare capacity, reallocation overlap, children,
            // and the result being assembled before allocating.
            memory.reserve(
                4 * (open.len() + leaves.len() + memory.branches + 1),
                forms.bytes() + seen.len() * memory.form + 2 * alike.bytes(),
            )?;
            let Some(test) = node.undecided.first() else {
                leaves.push((at, node));
                continue;
            };
            let mut children: Vec<(Node, Vec<Profile>)> = Vec::new();
            let split = tests.split(&node.profile, test);
            for child in split.into_iter().map(|profile| read(profile, Some(&node))) {
                match children
                    .iter_mut()
                    .find(|(known, _)| known.form == child.form)
                {
                    Some((first, others)) => {
                        // An example that stands for any value says nothing
                        // of the kind.
                        if test.any(&child.profile) {
                            first.profile.stand_ins.insert(test.name());
                        }
                        others.push(child.profile);
                    }
                    None => children.push((child, Vec::new())),
                }
            }
            for (child, others) in children {
                let like = match seen.get(&child.form) {
                    Some(&like) => {
                        alike.drop(at, test, &child.profile, like);
                        like
                    }
                    None => {
                        let id = alike.node(at, test, &child.profile);
                        seen.insert(child.form.clone(), id);
                        open.push((id, child));
                        id
                    }
                };
                for other in others {
                    alike.drop(at, test, &other, like);
                }
            }
        }
        // Each leaf's others are at most a node's worth.
        memory.reserve(
            4 * (leaves.len() + memory.branches + 1) + leaves.len(),
            forms.bytes() + seen.len() * memory.form + alike.bytes(),
        )?;
        let others = alike.others(&|bytes| {
            memory.reserve(
                4 * (leaves.len() + memory.branches + 1) + leaves.len(),
                forms.bytes() + seen.len() * memory.form + bytes,
            )
        })?;
        let mut kinds: Vec<(Vec<May>, Kind)> = Vec::new();
        let mut held = 0;
        for (at, node) in leaves {
            let others = alike.profile(&node.profile, &others[at]);
            let kind = Kind {
                examples: vec![node.profile],
                others: others.into_iter().collect(),
                asked: Arc::clone(&asked),
            };
            held += 1 + kind.others.len();
            let answers = node.answers;
            // A Maybe may resolve differently once a rule or record is known.
            match kinds
                .iter_mut()
                .find(|(known, _)| !answers.contains(&May::Maybe) && *known == answers)
            {
                Some((_, same)) => {
                    *same = std::mem::replace(same, Kind::of(Profile::default())).join(kind)
                }
                None => kinds.push((answers, kind)),
            }
        }
        std::mem::drop((alike, others));
        memory.reserve(
            4 * (kinds.len() + memory.branches + 1) + held,
            forms.bytes()
                + seen.len() * memory.form
                + memory::asked_heap(&asked)
                + kinds.len() * (size_of::<Answered>() + unknown.len() * size_of::<May>()),
        )?;
        // Each question's answer: someone unknown's when it is definite, else
        // that of the question that stood for it.
        let mut by_question: Vec<Option<usize>> = vec![None; unknown.len()];
        for (&at, &by) in maybe.iter().zip(&answered_by) {
            by_question[at] = Some(by);
        }
        Ok(kinds
            .into_iter()
            .map(|(answers, kind)| Answered {
                kind,
                answers: by_question
                    .iter()
                    .zip(&unknown)
                    .map(|(by, &known)| by.map_or(known, |by| answers[by]))
                    .collect(),
            })
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

fn facts(profile: &Profile, place: Place) -> &BTreeMap<String, Fact> {
    match place {
        Place::Principal => &profile.attrs,
        Place::Context => &profile.context,
    }
}

/// The questions a split asks, by action and resource type.
fn asked(questions: &[Question<'_>]) -> Asked {
    let mut asked = Asked::new();
    for question in questions {
        let causes = asked
            .entry(question.action.to_string())
            .or_default()
            .entry(question.resource_type.to_string())
            .or_default();
        if !causes.contains(&question.cause) {
            causes.push(question.cause.clone());
        }
    }
    asked
}
