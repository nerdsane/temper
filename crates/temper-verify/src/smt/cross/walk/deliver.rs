//! What is in flight arrives, in any order, until nothing is left.

use temper_spec::automaton::Action;
use z3::ast::{Bool, Int};

use super::{Firing, Flight, REORDER_UP_TO, Step, Walk};
use crate::smt::cross::world::{ABSENT, EMPTY, World};

impl<'a> Walk<'a> {
    /// Deliver what is in flight, in any order, then ask the rule.
    pub(super) fn settle(&mut self, world: World, flying: Vec<Flight>, path: Vec<Step>) {
        if self.cut || self.stop {
            return;
        }
        if flying.is_empty() {
            self.paths += 1;
            self.ask(&world, &path);
            return;
        }
        let orders = if flying.len() <= REORDER_UP_TO {
            flying.len()
        } else {
            1
        };
        for n in 0..orders {
            // Reactions to different types change different records.
            if flying[n].to != flying[0].to {
                continue;
            }
            let mut rest = flying.clone();
            let flight = rest.remove(n);
            self.deliver(&world, &flight, rest, &path);
        }
    }

    pub(super) fn deliver(
        &mut self,
        world: &World,
        flight: &Flight,
        rest: Vec<Flight>,
        path: &[Step],
    ) {
        let key = (flight.to.clone(), flight.action.clone());
        // The runtime stops a step its chain already holds.
        if flight.chain.contains(&key) {
            self.settle(world.clone(), rest, path.to_vec());
            return;
        }
        let Some(action) = self.scope.action(&flight.to, &flight.action) else {
            self.settle(world.clone(), rest, path.to_vec());
            return;
        };
        let targets = world.of(&flight.to);
        for &i in &targets {
            let rec = world.records[i].clone();
            self.solver.push();
            self.solver.assert(flight.id.eq(&rec.id));
            if flight.fresh {
                self.solver.assert(rec.exists.not());
            }
            if self.sat() {
                self.take(world, i, action, flight, &rest, path);
            }
            self.solver.pop(1);
            if self.cut {
                return;
            }
        }
        // Its target is a record of the window: any one can stand for it, and a
        // window that left it out would read it as never made. An empty
        // reference names no target, so nothing arrives.
        self.solver.push();
        self.solver.assert(Bool::or(&[
            flight.id.eq(Int::from_i64(EMPTY)),
            flight.id.eq(Int::from_i64(ABSENT)),
        ]));
        if self.sat() {
            self.settle(world.clone(), rest, path.to_vec());
        }
        self.solver.pop(1);
    }

    pub(super) fn take(
        &mut self,
        world: &World,
        i: usize,
        action: &'a Action,
        flight: &Flight,
        rest: &[Flight],
        path: &[Step],
    ) {
        let rec = &world.records[i].clone();
        // A reaction makes the record it reaches if the move starts from its
        // first status, whichever way its target was named.
        let made = self.enabled(rec, world, action, &flight.params);
        // Refused: nothing moves.
        self.solver.push();
        self.solver.assert(made.not());
        if self.sat() {
            self.settle(world.clone(), rest.to_vec(), path.to_vec());
        }
        self.solver.pop(1);
        self.solver.push();
        self.solver.assert(&made);
        if self.sat() {
            let (next, spawned) = self.apply(rec, action, &flight.params);
            let mut world = world.clone();
            world.records[i] = next;
            let mut path = path.to_vec();
            path.push(Step {
                text: format!(
                    "{}.{} arrives on {} (from {})",
                    flight.to, flight.action, rec.name, flight.from
                ),
                key: format!("{}.{}", flight.to, flight.action),
                shown: flight
                    .params
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            });
            let mut chain = flight.chain.clone();
            chain.push((flight.to.clone(), flight.action.clone()));
            let scope = self.scope;
            let ty: &'a str = scope
                .specs
                .get_key_value(&flight.to)
                .map_or("", |(k, _)| k.as_str());
            let sends = self.relevant_sends(ty, action);
            let firing = Firing {
                source: i,
                name: &rec.name,
                params: &flight.params,
                sends: &sends,
                chain: &chain,
                spawned: &spawned,
            };
            self.fire(world, &firing, 0, 0, rest.to_vec(), path);
        }
        self.solver.pop(1);
    }
}
