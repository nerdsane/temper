//! The context a declared answer is made in: a module's or an adapter's
//! answer, or a compensating move.

use super::AgentContext;

impl AgentContext {
    /// Made as whoever made the move that asked for the answer, with its
    /// budgets and trace and the answer's own key (from `for_callback`).
    /// The timer or scheduled action the asker fires belongs to that move;
    /// an answer is a move of its own and must not fire it again. It keeps
    /// [`AgentContext::ends_call`]: the answer removes its call's deadline.
    pub(crate) fn for_answer(&self) -> Self {
        Self {
            fires_scheduled_action: None,
            fires_state_timer: None,
            ..self.clone()
        }
    }
}
