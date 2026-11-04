//! Paths: each start, then everything it sets off, in each order.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use temper_spec::automaton::Action;
use z3::ast::{Bool, Int};

use super::{Firing, Flight, Step, Walk};
use crate::smt::cross::CrossRule;
use crate::smt::cross::scope::{Send, Target, own_reads, reads_of_send, sends_of};
use crate::smt::cross::world::{ABSENT, EMPTY, Rec, World};

impl<'a> Walk<'a> {
    /// Every way one of `breakers` can be the step: sent by someone or the
    /// kernel, or arriving from a sender with any values. When nothing here
    /// breaks the rule, no step can.
    pub fn check_breakers(&mut self, breakers: &BTreeSet<(String, String)>) {
        self.breakers = breakers.clone();
        let roots: Vec<(String, String)> = self
            .scope
            .roots
            .iter()
            .filter(|key| breakers.contains(*key))
            .cloned()
            .collect();
        self.start_all(&roots);
        for (ty, name, index) in self.scope.edges.clone() {
            let Some(action) = self.scope.action(&ty, &name) else {
                continue;
            };
            let send = sends_of(&ty, action)[index].clone();
            if !breakers.contains(&(send.to.to_string(), send.action.to_string())) {
                continue;
            }
            self.solver.push();
            self.stop = false;
            self.arrive(&ty, action, send);
            self.solver.pop(1);
        }
    }

    /// Follow each move that starts on its own and leads to a break found so
    /// far, so the break has a whole path. Stops as soon as each has one.
    pub fn check_chains(&mut self, breakers: &BTreeSet<(String, String)>) {
        let mut wanted: BTreeSet<String> = self.breaks.values().map(|b| b.at.clone()).collect();
        for found in self.breaks.values().filter(|b| b.from_start) {
            wanted.remove(&found.at);
        }
        self.wanted = Some(wanted);
        let roots: Vec<(String, String)> = self
            .scope
            .roots
            .iter()
            .filter(|key| !breakers.contains(*key))
            .cloned()
            .collect();
        for root in roots {
            if self.wanted.as_ref().is_some_and(BTreeSet::is_empty) {
                break;
            }
            self.start_all(std::slice::from_ref(&root));
        }
    }

    pub(super) fn start_all(&mut self, roots: &[(String, String)]) {
        let start_on = self.start_on.clone();
        for (ty, name) in roots {
            let Some(action) = self.scope.action(ty, name) else {
                continue;
            };
            let on: Vec<usize> = start_on
                .iter()
                .copied()
                .filter(|i| &self.before.records[*i].ty == ty)
                .collect();
            for i in on {
                self.solver.push();
                self.stop = false;
                let params = self.free_params(action);
                self.start(i, ty, action, params);
                self.solver.pop(1);
            }
        }
    }

    /// Stop when the time runs out from here on, and clear what the last
    /// phase ran out of.
    pub fn until(&mut self, deadline: Option<Instant>) {
        self.deadline = deadline;
        self.cut = false;
    }

    pub(super) fn free_params(&mut self, action: &Action) -> BTreeMap<String, Int> {
        action
            .params
            .iter()
            .map(|p| (p.name().to_string(), Int::fresh_const(p.name())))
            .collect()
    }

    /// `action` taken on record `i` from outside, or by the kernel.
    pub(super) fn start(
        &mut self,
        i: usize,
        ty: &str,
        action: &Action,
        params: BTreeMap<String, Int>,
    ) {
        let world = self.before.clone();
        let rec = world.records[i].clone();
        self.not_a_minted_id(&rec);
        let enabled = self.enabled(&rec, &world, action, &params);
        self.solver.assert(&enabled);
        if !self.sat() {
            return;
        }
        let (next, spawned) = self.apply(&rec, action, &params);
        let mut world = world;
        world.records[i] = next;
        let step = Step {
            text: format!("{}.{} on {}", ty, action.name, rec.name),
            key: format!("{ty}.{}", action.name),
            shown: params.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        };
        let chain = vec![(ty.to_string(), action.name.clone())];
        let sends = self.relevant_sends(ty, action);
        let firing = Firing {
            source: i,
            name: &rec.name,
            params: &params,
            sends: &sends,
            chain: &chain,
            spawned: &spawned,
        };
        self.fire(world, &firing, 0, 0, Vec::new(), vec![step]);
    }

    pub(super) fn relevant_sends(&self, ty: &'a str, action: &'a Action) -> Vec<Send<'a>> {
        sends_of(ty, action)
            .into_iter()
            .filter(|send| {
                self.scope
                    .relevant
                    .contains(&(send.to.to_string(), send.action.to_string()))
            })
            .collect()
    }

    /// One reaction arriving from a sender that made its move some time ago:
    /// the sender's values are free, within its move's places and the guard.
    pub(super) fn arrive(&mut self, ty: &str, action: &Action, send: Send<'_>) {
        let spec = self.spec(ty);
        let mut reads = BTreeSet::new();
        reads_of_send(&send, &mut reads);
        let links: Vec<CrossRule> = self
            .links
            .iter()
            .filter(|rule| rule.owner == ty)
            .cloned()
            .collect();
        for rule in &links {
            own_reads(&rule.assert, &mut reads);
        }
        reads.remove("Id");
        reads.remove("id");
        let name = format!("sender {ty}.{}", action.name);
        let fields: BTreeMap<String, Int> = reads
            .iter()
            .map(|f| (f.clone(), Int::fresh_const(f)))
            .collect();
        let id = Int::fresh_const("sender_id");
        self.solver.assert(id.eq(Int::from_i64(EMPTY)).not());
        self.solver.assert(id.eq(Int::from_i64(ABSENT)).not());
        let status = Int::fresh_const("sender_status");
        let places: Vec<String> = match &action.to {
            Some(to) => vec![to.clone()],
            None if action.from.is_empty() => {
                spec.map(|s| s.automaton.states.clone()).unwrap_or_default()
            }
            None => action.from.clone(),
        };
        let at: Vec<Bool> = places
            .iter()
            .map(|p| status.eq(Int::from_i64(self.atoms.atom(p))))
            .collect();
        self.solver.assert(Bool::or(&at));
        let ghost = Rec {
            name: name.clone(),
            ty: ty.to_string(),
            id,
            exists: Bool::from_bool(true),
            status,
            fields,
        };
        if let Some(spec) = spec {
            self.limit_fields(&ghost, spec);
        }
        let params = self.free_params(action);
        let world = self.before.clone();
        for rule in &links {
            let holds = self.read(&ghost, &world, None, &rule.assert);
            self.solver.assert(&holds);
        }
        // A reaction a record sends itself reads that record, not a stranger.
        if matches!(send.target, Target::Same) {
            for i in world.of(ty) {
                let rec = world.records[i].clone();
                self.solver.push();
                self.solver.assert(ghost.id.eq(&rec.id));
                self.solver.assert(ghost.status.eq(&rec.status));
                for (field, value) in &ghost.fields {
                    if rec.fields.contains_key(field) {
                        self.solver.assert(value.eq(rec.field(field)));
                    }
                }
                if self.sat() {
                    self.send_from(action, ty, &send, world.clone(), ghost.clone(), &params);
                }
                self.solver.pop(1);
            }
            return;
        }
        self.send_from(action, ty, &send, world, ghost, &params);
    }

    /// What a sender from before the step has sent now arrives.
    pub(super) fn send_from(
        &mut self,
        action: &Action,
        ty: &str,
        send: &Send<'_>,
        world: World,
        ghost: Rec,
        params: &BTreeMap<String, Int>,
    ) {
        let mut spawned = Vec::new();
        if let Target::Spawn { store, .. } = &send.target {
            let child = self.minted(send.to, &ghost);
            // What it stored when it sent this is what now arrives.
            if let Some(held) = store.as_ref().and_then(|field| ghost.fields.get(field)) {
                self.solver.assert(held.eq(&child));
            }
            spawned.push(child);
        }
        let step = Step {
            text: format!("{ty}.{} sent {} earlier", action.name, send.name),
            key: format!("~{ty}.{}", action.name),
            shown: ghost
                .fields
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        };
        let chain = vec![(ty.to_string(), action.name.clone())];
        // The sender stands outside the records: at the end of the list while
        // its reaction goes out, and gone before anything arrives.
        let mut world = world;
        world.records.push(ghost.clone());
        let firing = Firing {
            source: world.records.len() - 1,
            name: &ghost.name,
            params,
            sends: std::slice::from_ref(send),
            chain: &chain,
            spawned: &spawned,
        };
        self.fire(world, &firing, 0, 0, Vec::new(), vec![step]);
    }

    /// Decide reaction `k` of the moved record and those after it, then settle.
    /// `spawn_at` is how many of them were spawns.
    pub(super) fn fire(
        &mut self,
        world: World,
        firing: &Firing<'_, '_>,
        k: usize,
        spawn_at: usize,
        flying: Vec<Flight>,
        path: Vec<Step>,
    ) {
        let i = firing.source;
        let Some(send) = firing.sends.get(k) else {
            let mut world = world;
            // A sender from before the step is not a record of the world.
            if i >= self.before.records.len() {
                world.records.truncate(self.before.records.len());
            }
            self.settle(world, flying, path);
            return;
        };
        let next_spawn = spawn_at + usize::from(matches!(send.target, Target::Spawn { .. }));
        let guard = send.guard.map(|g| {
            let rec = world.records[i].clone();
            self.read(&rec, &world, None, g)
        });
        if let Some(guard) = &guard {
            // It does not fire.
            self.solver.push();
            self.solver.assert(guard.not());
            if self.sat() {
                self.fire(
                    world.clone(),
                    firing,
                    k + 1,
                    next_spawn,
                    flying.clone(),
                    path.clone(),
                );
            }
            self.solver.pop(1);
        }
        self.solver.push();
        if let Some(guard) = &guard {
            self.solver.assert(guard);
        }
        if self.sat() {
            let mut world = world;
            let mut flying = flying;
            let flight = self.flight(&mut world, firing, send, firing.spawned.get(spawn_at));
            flying.push(flight);
            self.fire(world, firing, k + 1, next_spawn, flying, path);
        }
        self.solver.pop(1);
    }

    pub(super) fn flight(
        &mut self,
        world: &mut World,
        firing: &Firing<'_, '_>,
        send: &Send<'_>,
        child: Option<&Int>,
    ) -> Flight {
        let i = firing.source;
        let params = firing.params;
        let mut rec = world.records[i].clone();
        let (id, fresh) = match &send.target {
            Target::Field(field) => (rec.field(field), false),
            Target::Same => (rec.id.clone(), false),
            Target::Static(id) => (Int::from_i64(self.atoms.atom(id)), false),
            Target::MakeAt(field) => {
                let held = rec.field(field);
                let new = self.minted(send.to, &rec);
                let blank = Bool::or(&[
                    held.eq(Int::from_i64(EMPTY)),
                    held.eq(Int::from_i64(ABSENT)),
                ]);
                let id = blank.ite(&new, &held);
                if rec.fields.contains_key(field) {
                    rec.fields.insert(field.clone(), id.clone());
                    world.records[i] = rec.clone();
                }
                (id, false)
            }
            Target::Make => (self.minted(send.to, &rec), true),
            Target::Spawn { id, .. } => match child {
                Some(child) => (child.clone(), id.is_none()),
                None => (self.minted(send.to, &rec), true),
            },
        };
        let mut out = BTreeMap::new();
        if send.passes_params {
            out.extend(params.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        for (name, arg) in &send.args {
            let value = self.arg_value(&rec, params, arg);
            out.insert(name.clone(), value);
        }
        Flight {
            to: send.to.to_string(),
            action: send.action.to_string(),
            id,
            params: out,
            fresh,
            chain: firing.chain.to_vec(),
            from: format!("{} by {}", firing.name, send.name),
        }
    }
}
