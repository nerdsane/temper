//! The cause a timer's fire carries.
//!
//! A timer fires as whoever armed it, and Cedar is not asked about its own
//! move: the arming move already passed Cedar to enter the timed state, and
//! the timer is that state's own consequence in the spec. Cedar is asked,
//! under this cause, whether the one who armed it has lost their standing on
//! the record since (`timer_standing`). The moves its move sets off are
//! checked, and they
//! inherit this cause, so Cedar sees them as `via == "timer"`, armed by the
//! timer's principal.

use temper_authz::{MoveCause, TimerArm, TimerRecord};

use crate::request_context::AgentContext;

/// Mark `agent`, the identity a timer fires as, as that timer's fire, armed
/// by its own principal. It replaces any cause the arming move carried: a
/// timer's chain starts at its fire. A context with no security context (an
/// internal caller) carries no cause. `state_timer_of` is the record whose
/// state timer fires; None for a scheduled action.
pub(super) fn mark_timer_fire(agent: &mut AgentContext, state_timer_of: Option<TimerRecord>) {
    if let Some(security_ctx) = agent.security_ctx.as_mut() {
        security_ctx.cause = Some(MoveCause::timer_fire(TimerArm {
            armed_by: security_ctx.principal.id.clone(),
            state_timer_of,
        }));
    }
}
