use super::TriggerGuard;

impl TriggerGuard {
    /// Compute the maximum composite nesting depth of this guard.
    ///
    /// Leaf variants are depth 1. `AllOf` / `AnyOf` / `Not` add one level.
    /// Used at parse time to reject guards deeper than
    /// `MAX_TRIGGER_GUARD_DEPTH`.
    pub fn depth(&self) -> u32 {
        match self {
            TriggerGuard::FieldEquals { .. }
            | TriggerGuard::FieldIn { .. }
            | TriggerGuard::BoolTrue { .. }
            | TriggerGuard::BoolFalse { .. }
            | TriggerGuard::StateIn { .. }
            | TriggerGuard::CrossEntityStateIn { .. } => 1,
            TriggerGuard::AllOf { guards } | TriggerGuard::AnyOf { guards } => {
                1 + guards.iter().map(Self::depth).max().unwrap_or(0)
            }
            TriggerGuard::Not { guard } => 1 + guard.depth(),
        }
    }
}
