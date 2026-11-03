//! How a move came about, as Cedar is told it.
//!
//! A move is made by a direct request, by a reaction a spec's
//! `[[action.triggers]]` sets off, or by a timer: a state's `state_timeout`
//! or a `schedule` effect. The principal is the true one on every path: a
//! reaction runs as whoever made the move before it (or the service its rule
//! declares), and a timer fires as whoever armed it. So an app's Cedar cannot
//! tell from the principal whether a hook picked a move itself or the spec
//! made it follow the hook's own move. [`MoveCause`] says which, and
//! [`cause_context`] turns it into the Cedar context of every check: the keys
//! in [`platform_keys::CAUSE`](crate::platform_keys::CAUSE), which only the
//! kernel sets.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::platform_keys;

/// How a move that is not a direct request came about, carried on the
/// [`SecurityContext`](crate::SecurityContext) it is checked with. A direct
/// request carries none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveCause {
    /// The rule that made this move and the move that set it off, when this
    /// move is a reaction; None for a timer's own fire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reaction: Option<ReactionHop>,
    /// Reaction hops since the request or the timer fire the chain started
    /// from: 0 for a timer's own fire, 1 for a reaction to a request or to a
    /// timer's fire, one more for each further hop.
    pub depth: u32,
    /// The timer whose fire started this chain, when one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timer: Option<TimerArm>,
}

/// One reaction hop: the rule, and the move that set it off.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReactionHop {
    /// The kernel's name for the rule, as logs and reaction results name it:
    /// `<Type>:<Action>:<trigger>` for a spec's `[[action.triggers]]`.
    pub rule: String,
    /// The move that set the reaction off.
    pub action: String,
    /// That move's entity type.
    pub entity_type: String,
    /// That move's entity id.
    pub entity_id: String,
}

/// The arming of the timer a chain started from.
///
/// It names who armed the timer, not the move that did: a state timer keeps its
/// clock and identity when a later move in its state re-arms it, and one armed
/// at a spec install has no arming move, so no move can be named truthfully.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimerArm {
    /// The id of the principal that armed it, which its fire runs as.
    pub armed_by: String,
    /// The record whose state timer this is; None for a scheduled action.
    /// A refusal late in the chain, an integration's answer's reaction, say,
    /// finds the timer that set it off from this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_timer_of: Option<TimerRecord>,
}

/// The record a state timer belongs to, and which of its declared timers it
/// is: a refusal late in its chain stops only that timer, not one the record
/// moved on to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimerRecord {
    /// The record's entity type.
    pub entity_type: String,
    /// The record's entity id.
    pub entity_id: String,
    /// The timed state the timer was declared for.
    #[serde(default)]
    pub state: String,
    /// The timer's `on_timeout` action.
    #[serde(default)]
    pub action: String,
}

impl MoveCause {
    /// A timer's own fire.
    pub fn timer_fire(arm: TimerArm) -> Self {
        Self {
            reaction: None,
            depth: 0,
            timer: Some(arm),
        }
    }

    /// The reaction `hop` makes to a move whose cause was `source` (None for
    /// a direct request): one hop deeper, in the same timer's chain if any.
    pub fn reaction(hop: ReactionHop, source: Option<&MoveCause>) -> Self {
        Self {
            reaction: Some(hop),
            depth: source.map_or(0, |cause| cause.depth) + 1,
            timer: source.and_then(|cause| cause.timer.clone()),
        }
    }

    /// `"timer"` anywhere in a timer's chain, else `"reaction"`.
    pub fn via(&self) -> &'static str {
        if self.timer.is_some() {
            "timer"
        } else {
            "reaction"
        }
    }
}

/// The Cedar context a check gets from its move's cause: `via` and `depth`
/// always, the reaction's hop on a reaction, the timer's arm in a timer's
/// chain. A check with no cause is a direct request: `via == "request"`,
/// `depth == 0`, so a path the kernel does not mark gets the strict case.
pub(crate) fn cause_context(cause: Option<&MoveCause>) -> Vec<(&'static str, Value)> {
    let string = |value: &str| Value::String(value.to_string());
    let Some(cause) = cause else {
        return vec![
            (platform_keys::VIA, string("request")),
            (platform_keys::DEPTH, Value::from(0)),
        ];
    };
    let mut context = vec![
        (platform_keys::VIA, string(cause.via())),
        (platform_keys::DEPTH, Value::from(cause.depth)),
    ];
    if let Some(hop) = &cause.reaction {
        context.push((platform_keys::RULE, string(&hop.rule)));
        context.push((platform_keys::CAUSE_ACTION, string(&hop.action)));
        context.push((platform_keys::CAUSE_ENTITY_TYPE, string(&hop.entity_type)));
        context.push((platform_keys::CAUSE_ENTITY_ID, string(&hop.entity_id)));
    }
    if let Some(timer) = &cause.timer {
        context.push((platform_keys::ARMED_BY, string(&timer.armed_by)));
    }
    debug_assert!(
        context
            .iter()
            .all(|(key, _)| platform_keys::CAUSE.contains(key))
    );
    context
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hop(rule: &str, action: &str) -> ReactionHop {
        ReactionHop {
            rule: rule.to_string(),
            action: action.to_string(),
            entity_type: "Direction".to_string(),
            entity_id: "d1".to_string(),
        }
    }

    fn arm() -> TimerArm {
        TimerArm {
            armed_by: "hook-1".to_string(),
            state_timer_of: None,
        }
    }

    #[test]
    fn a_request_is_via_request_at_depth_zero() {
        assert_eq!(
            cause_context(None),
            vec![("via", Value::from("request")), ("depth", Value::from(0))]
        );
    }

    #[test]
    fn each_reaction_hop_is_one_deeper() {
        let first = MoveCause::reaction(hop("Direction:FindFix:goes", "FindFix"), None);
        let second = MoveCause::reaction(hop("Direction:AutoGo:lands", "AutoGo"), Some(&first));
        assert_eq!((first.depth, second.depth), (1, 2));
        assert_eq!(second.via(), "reaction");
        let context = cause_context(Some(&second));
        assert!(context.contains(&("rule", Value::from("Direction:AutoGo:lands"))));
        assert!(context.contains(&("causeAction", Value::from("AutoGo"))));
        assert!(!context.iter().any(|(key, _)| *key == "armedBy"));
    }

    #[test]
    fn a_timers_chain_is_via_timer_and_keeps_its_arm() {
        let fire = MoveCause::timer_fire(arm());
        assert_eq!((fire.via(), fire.depth), ("timer", 0));
        let reaction = MoveCause::reaction(hop("Direction:Lapse:follows_up", "Lapse"), Some(&fire));
        assert_eq!(reaction.depth, 1);
        assert_eq!(reaction.timer, Some(arm()));
        let context = cause_context(Some(&reaction));
        for expected in [
            ("via", Value::from("timer")),
            ("depth", Value::from(1)),
            ("armedBy", Value::from("hook-1")),
            ("rule", Value::from("Direction:Lapse:follows_up")),
            ("causeAction", Value::from("Lapse")),
        ] {
            assert!(context.contains(&expected), "{expected:?} in {context:?}");
        }
    }

    /// A module's call during a reaction is bound with that reaction's
    /// context, and must still be checked as the module's request.
    #[test]
    fn a_request_bound_with_a_moves_context_carries_no_cause() {
        let mut during_reaction = crate::SecurityContext::from_verified_jwt(
            "hook-1",
            crate::PrincipalKind::Agent,
            Some("katagami-hook"),
            Some("owner-1"),
            None,
            None,
        );
        during_reaction.cause = Some(MoveCause::reaction(
            ReactionHop {
                rule: "Direction:FindFix:goes".to_string(),
                action: "FindFix".to_string(),
                entity_type: "Direction".to_string(),
                entity_id: "d1".to_string(),
            },
            None,
        ));
        let request = crate::AuthenticatedRequestContext::new(
            temper_runtime::tenant::TenantId::default(),
            during_reaction,
        );
        assert_eq!(request.security_context().cause, None);
        assert_eq!(request.security_context().principal.id, "hook-1");
    }

    #[test]
    fn a_cause_stored_with_a_timers_identity_reads_back() {
        let reaction = MoveCause::reaction(hop("Direction:FindFix:goes", "FindFix"), None);
        for cause in [reaction, MoveCause::timer_fire(arm())] {
            let json = serde_json::to_value(&cause).unwrap();
            assert_eq!(serde_json::from_value::<MoveCause>(json).unwrap(), cause);
        }
    }
}
