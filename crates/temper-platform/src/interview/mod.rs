//! Developer interview module for entity discovery and spec generation.
//!
//! Guides developers through a structured conversation to discover entities,
//! states, actions, guards, and invariants. Produces IOA TOML, CSDL XML, and
//! Cedar policies from the collected entity models.

pub mod entity_collector;
pub mod spec_generator;

pub use entity_collector::{
    ActionDefinition, ActionKind, EntityModel, InvariantDefinition, StateDefinition, StateVariable,
};
pub use spec_generator::{generate_cedar_policies, generate_csdl_xml, generate_ioa_toml};

use serde::{Deserialize, Serialize};

/// Phases of the developer interview flow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InterviewPhase {
    /// Initial greeting and project description.
    Welcome,
    /// Discover entity types the application needs.
    EntityDiscovery,
    /// Discover states for each entity.
    StateDiscovery,
    /// Discover actions and transitions.
    ActionDiscovery,
    /// Discover guards and preconditions.
    GuardDiscovery,
    /// Discover safety invariants.
    InvariantDiscovery,
    /// Review generated specs before verification.
    SpecReview,
    /// Running the verification cascade.
    Verifying,
    /// Successfully deployed.
    Deployed,
}

impl InterviewPhase {
    /// Advance to the next interview phase, or `None` if already at the end.
    pub fn next(&self) -> Option<InterviewPhase> {
        match self {
            Self::Welcome => Some(Self::EntityDiscovery),
            Self::EntityDiscovery => Some(Self::StateDiscovery),
            Self::StateDiscovery => Some(Self::ActionDiscovery),
            Self::ActionDiscovery => Some(Self::GuardDiscovery),
            Self::GuardDiscovery => Some(Self::InvariantDiscovery),
            Self::InvariantDiscovery => Some(Self::SpecReview),
            Self::SpecReview => Some(Self::Verifying),
            Self::Verifying => Some(Self::Deployed),
            Self::Deployed => None,
        }
    }

    /// Progress percentage through the interview (0-100).
    pub fn progress_percent(&self) -> u8 {
        match self {
            Self::Welcome => 0,
            Self::EntityDiscovery => 12,
            Self::StateDiscovery => 25,
            Self::ActionDiscovery => 37,
            Self::GuardDiscovery => 50,
            Self::InvariantDiscovery => 62,
            Self::SpecReview => 75,
            Self::Verifying => 87,
            Self::Deployed => 100,
        }
    }

    /// Human-readable name for this phase.
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Welcome => "Welcome",
            Self::EntityDiscovery => "Entity Discovery",
            Self::StateDiscovery => "State Discovery",
            Self::ActionDiscovery => "Action Discovery",
            Self::GuardDiscovery => "Guard Discovery",
            Self::InvariantDiscovery => "Invariant Discovery",
            Self::SpecReview => "Spec Review",
            Self::Verifying => "Verifying",
            Self::Deployed => "Deployed",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_phase_progression() {
        let mut phase = InterviewPhase::Welcome;
        let expected = [
            InterviewPhase::EntityDiscovery,
            InterviewPhase::StateDiscovery,
            InterviewPhase::ActionDiscovery,
            InterviewPhase::GuardDiscovery,
            InterviewPhase::InvariantDiscovery,
            InterviewPhase::SpecReview,
            InterviewPhase::Verifying,
            InterviewPhase::Deployed,
        ];
        for expected_next in &expected {
            let next = phase.next().expect("should have a next phase");
            assert_eq!(&next, expected_next);
            phase = next;
        }
        assert_eq!(phase.next(), None, "Deployed should be terminal");
    }

    #[test]
    fn test_phase_progress_percent() {
        assert_eq!(InterviewPhase::Welcome.progress_percent(), 0);
        assert_eq!(InterviewPhase::Deployed.progress_percent(), 100);
        // Monotonically increasing
        let phases = [
            InterviewPhase::Welcome,
            InterviewPhase::EntityDiscovery,
            InterviewPhase::StateDiscovery,
            InterviewPhase::ActionDiscovery,
            InterviewPhase::GuardDiscovery,
            InterviewPhase::InvariantDiscovery,
            InterviewPhase::SpecReview,
            InterviewPhase::Verifying,
            InterviewPhase::Deployed,
        ];
        for window in phases.windows(2) {
            assert!(
                window[0].progress_percent() < window[1].progress_percent(),
                "{:?} ({}) should be < {:?} ({})",
                window[0],
                window[0].progress_percent(),
                window[1],
                window[1].progress_percent(),
            );
        }
    }
}
