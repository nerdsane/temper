//! The answer a refused spec change gives: the `[[carry]]` sections that
//! would fix it first, ready to paste, then the records and why.

use std::collections::BTreeSet;
use std::fmt;

use temper_jit::table::{Blocked, EvalContext, Standing, TransitionTable};
use temper_spec::automaton::is_carry_move;

/// How many ids a line of the answer names before it gives the rest as a
/// count, so the answer stays readable; [`StuckRecords::ids`] holds them all.
const IDS_SHOWN: usize = 20;

/// Stored records of one type in one state that a new version leaves with no
/// way forward.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StuckRecords {
    /// Their type.
    pub entity_type: String,
    /// The state they are in.
    pub status: String,
    /// Their ids, in order.
    pub ids: Vec<String>,
    /// Why they have no way forward.
    pub why: String,
}

/// Why the new versions may not be installed. Nothing was written.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpecChangeRefused {
    /// `[[carry]]` sections that would carry the stuck records, one per type
    /// and state, each headed by a comment naming the type's spec.
    pub carries: Vec<String>,
    /// The records left with no way forward.
    pub stuck: Vec<StuckRecords>,
    /// Every other reason: a carry that cannot carry its records, or records
    /// that could not be read.
    pub reasons: Vec<String>,
}

impl SpecChangeRefused {
    /// Whether nothing stops the install.
    pub fn is_empty(&self) -> bool {
        self.carries.is_empty() && self.stuck.is_empty() && self.reasons.is_empty()
    }

    /// Add the records of `entity_type` in `status` that `new` leaves with no
    /// way forward, standing as `now`, with the carry that would move them.
    pub(super) fn add_stuck(
        &mut self,
        entity_type: &str,
        (old, new): (&TransitionTable, &TransitionTable),
        status: &str,
        now: &Standing,
        ids: Vec<String>,
    ) {
        let records = count(ids.len());
        let (why, carry) = match now {
            Standing::Stuck(blocked) => (
                format!(
                    "no move is possible for them any more: {}",
                    blocked_moves(blocked)
                ),
                match wanted_values(blocked) {
                    Some(set) => format!("from = \"{status}\"\nset = {{ {set} }}"),
                    None => format!("from = \"{status}\"\n{}", pick_target(new)),
                },
            ),
            _ => (
                format!("{status} is not a state of the new version"),
                match obvious_target(old, new, status) {
                    Some(target) => format!("from = \"{status}\"\nto = \"{target}\""),
                    None => format!("from = \"{status}\"\n{}", pick_target(new)),
                },
            ),
        };
        self.carries.push(format!(
            "# {entity_type}: {records} in {status}\n[[carry]]\n{carry}"
        ));
        self.stuck.push(StuckRecords {
            entity_type: entity_type.to_string(),
            status: status.to_string(),
            ids,
            why,
        });
    }
}

impl fmt::Display for SpecChangeRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.carries.is_empty() {
            writeln!(
                f,
                "The new version leaves stored records with no way forward. Add these \
                 [[carry]] sections to say where they go, or change the version so they keep \
                 a move:\n"
            )?;
            for carry in &self.carries {
                writeln!(f, "{carry}\n")?;
            }
        }
        let mut lines: Vec<String> = self
            .stuck
            .iter()
            .map(|stuck| {
                with_records(
                    &format!("{} in {}: {}", stuck.entity_type, stuck.status, stuck.why),
                    &stuck.ids,
                )
            })
            .collect();
        lines.extend(self.reasons.iter().cloned());
        f.write_str(&lines.join("\n"))
    }
}

impl std::error::Error for SpecChangeRefused {}

/// `note` with how many records it is about and their ids.
pub(super) fn with_records(note: &str, ids: &[String]) -> String {
    let shown = ids
        .iter()
        .take(IDS_SHOWN)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    let more = match ids.len().saturating_sub(IDS_SHOWN) {
        0 => String::new(),
        rest => format!(" and {rest} more"),
    };
    format!("{note}. {}: {shown}{more}", count(ids.len()))
}

/// The moves a record in `status` could make under `old` and cannot under
/// `new`, though it keeps another way forward, as a note; `None` when it
/// lost none.
pub(super) fn lost_moves(
    entity_type: &str,
    (old, new): (&TransitionTable, &TransitionTable),
    status: &str,
    (before, after): (&EvalContext, &EvalContext),
) -> Option<String> {
    let lost: Vec<&str> = old
        .caller_moves()
        .into_iter()
        .filter(|action| {
            old.starts_from(action, status)
                && old.blocked(status, before, action).is_none()
                && new.blocked(status, after, action).is_some()
        })
        .collect();
    (!lost.is_empty()).then(|| {
        format!(
            "{entity_type} in {status}: these records can no longer {} and keep another way \
             forward",
            lost.join(" or ")
        )
    })
}

/// Each blocked move with why, in words.
pub(super) fn blocked_moves(blocked: &[(String, Blocked)]) -> String {
    blocked
        .iter()
        .map(|(action, why)| format!("{action}: {}", blocked_why(why)))
        .collect::<Vec<_>>()
        .join("; ")
}

fn blocked_why(blocked: &Blocked) -> String {
    match blocked {
        Blocked::NoAction => "the move is gone".into(),
        Blocked::NotFrom => "it does not start here".into(),
        Blocked::Guard(Some(failure)) if failure.found.is_empty() => {
            format!("'{}' is false", failure.expr)
        }
        Blocked::Guard(Some(failure)) => {
            let found = failure
                .found
                .iter()
                .map(|(name, value)| format!("{name} = {value}"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("'{}' is false ({found})", failure.expr)
        }
        Blocked::Guard(None) => "its guard is false".into(),
    }
}

/// The values that would make the first move whose guard is false possible,
/// as TOML: each `name == value` comparison of its failing part. `None`
/// when that part has none.
fn wanted_values(blocked: &[(String, Blocked)]) -> Option<String> {
    blocked.iter().find_map(|(_, why)| {
        let Blocked::Guard(Some(failure)) = why else {
            return None;
        };
        let set: Vec<String> = failure
            .conditions
            .iter()
            .filter(|condition| condition.op == "==" && condition.field != "status")
            .map(|condition| format!("{} = {}", condition.field, condition.wants))
            .collect();
        (!set.is_empty()).then(|| set.join(", "))
    })
}

/// Where a dropped state's records obviously go: where the installed
/// version's carry from `status` takes them, the one state `new` adds when
/// it drops exactly one, or else the one state every move of `old` from
/// `status` led to.
fn obvious_target(old: &TransitionTable, new: &TransitionTable, status: &str) -> Option<String> {
    let carried_to = old
        .rules
        .iter()
        .find(|rule| is_carry_move(&rule.name) && rule.from_states.iter().any(|s| s == status))
        .and_then(|rule| rule.to_state.as_deref());
    if let Some(target) = carried_to.filter(|target| new.states.iter().any(|s| s == target)) {
        return Some(target.to_string());
    }
    let dropped = old
        .states
        .iter()
        .filter(|state| !new.states.contains(state));
    let added: Vec<&String> = new
        .states
        .iter()
        .filter(|state| !old.states.contains(state))
        .collect();
    if dropped.count() == 1 && added.len() == 1 {
        return Some(added[0].clone());
    }
    let led_to: BTreeSet<&str> = old
        .rules
        .iter()
        .filter(|rule| {
            !is_carry_move(&rule.name) && rule.from_states.iter().any(|from| from == status)
        })
        .filter_map(|rule| rule.to_state.as_deref())
        .collect();
    match led_to.into_iter().collect::<Vec<_>>()[..] {
        [target] if target != status && new.states.iter().any(|state| state == target) => {
            Some(target.to_string())
        }
        _ => None,
    }
}

/// A `to` line for the author to finish, naming the states to pick from.
fn pick_target(new: &TransitionTable) -> String {
    format!("to = \"?\"  # one of: {}", new.states.join(", "))
}

fn count(records: usize) -> String {
    match records {
        1 => "1 record".to_string(),
        n => format!("{n} records"),
    }
}
