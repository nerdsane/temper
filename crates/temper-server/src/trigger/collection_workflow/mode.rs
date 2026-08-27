//! Operational activation mode for public collection workflows.

/// Process-wide collection-workflow activation mode.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum CollectionWorkflowMode {
    /// Declarations, starts, controls, recovery, joins, and Observe are active.
    Enabled,
    /// New starts are rejected while existing work can quiesce.
    Draining,
    /// New declarations and starts are rejected after quiescence.
    #[default]
    Disabled,
}

impl CollectionWorkflowMode {
    /// Parse the exact supported configuration vocabulary.
    pub fn parse(value: Option<&str>) -> Result<Self, String> {
        match value {
            None => Ok(Self::Disabled),
            Some("enabled") => Ok(Self::Enabled),
            Some("draining") => Ok(Self::Draining),
            Some("disabled") => Ok(Self::Disabled),
            Some(other) => Err(format!(
                "invalid TEMPER_COLLECTION_WORKFLOW_MODE '{other}'; expected enabled, draining, or disabled"
            )),
        }
    }

    /// Read and validate process configuration once during startup.
    pub fn from_env() -> Result<Self, String> {
        let value = std::env::var("TEMPER_COLLECTION_WORKFLOW_MODE").ok(); // determinism-ok: startup-only process configuration
        Self::parse(value.as_deref())
    }

    /// Reject a new workflow start with its stable public error code.
    pub fn require_start_enabled(self) -> Result<(), &'static str> {
        match self {
            Self::Enabled => Ok(()),
            Self::Draining => Err("CollectionWorkflowDraining"),
            Self::Disabled => Err("CollectionWorkflowDisabled"),
        }
    }

    /// Admit collection declarations only while authoring is enabled.
    ///
    /// Draining and disabled processes may reload an identical installed
    /// declaration for recovery, but cannot introduce or alter one.
    pub fn require_declaration_enabled(
        self,
        declaration_unchanged: bool,
    ) -> Result<(), &'static str> {
        if declaration_unchanged || matches!(self, Self::Enabled) {
            return Ok(());
        }
        match self {
            Self::Enabled => Ok(()),
            Self::Draining => Err("CollectionWorkflowDraining"),
            Self::Disabled => Err("CollectionWorkflowDisabled"),
        }
    }

    /// Gate an incoming complete IOA source while allowing byte-identical
    /// recovery reloads of an already installed collection spec.
    pub fn require_spec_source(
        self,
        existing_source: Option<&str>,
        incoming_source: &str,
    ) -> Result<(), String> {
        if matches!(self, Self::Enabled) {
            return Ok(());
        }
        if existing_source == Some(incoming_source) {
            return Ok(());
        }
        if !declares_collection(existing_source.unwrap_or_default())
            && !declares_collection(incoming_source)
        {
            return Ok(());
        }
        self.require_declaration_enabled(false)
            .map_err(str::to_string)
    }

    /// Whether governed collection Observe routes remain available.
    pub const fn observe_enabled(self) -> bool {
        !matches!(self, Self::Disabled)
    }
}

fn declares_collection(source: &str) -> bool {
    match temper_spec::automaton::parse_automaton(source) {
        Ok(automaton) => !automaton.collection_workflows.is_empty(),
        Err(_) => source
            .lines()
            .any(|line| line.trim() == "[[collection_workflow]]"),
    }
}

#[cfg(test)]
mod tests {
    use super::CollectionWorkflowMode;

    #[test]
    fn mode_defaults_disabled_and_accepts_exact_vocabulary() {
        assert_eq!(
            CollectionWorkflowMode::parse(None),
            Ok(CollectionWorkflowMode::Disabled)
        );
        assert_eq!(
            CollectionWorkflowMode::parse(Some("enabled")),
            Ok(CollectionWorkflowMode::Enabled)
        );
        assert_eq!(
            CollectionWorkflowMode::parse(Some("draining")),
            Ok(CollectionWorkflowMode::Draining)
        );
        assert_eq!(
            CollectionWorkflowMode::parse(Some("disabled")),
            Ok(CollectionWorkflowMode::Disabled)
        );
        assert!(CollectionWorkflowMode::parse(Some("ENABLED")).is_err());
        assert!(CollectionWorkflowMode::parse(Some("other")).is_err());
    }

    #[test]
    fn only_enabled_mode_admits_new_starts() {
        assert!(
            CollectionWorkflowMode::Enabled
                .require_start_enabled()
                .is_ok()
        );
        assert_eq!(
            CollectionWorkflowMode::Draining.require_start_enabled(),
            Err("CollectionWorkflowDraining")
        );
        assert_eq!(
            CollectionWorkflowMode::Disabled.require_start_enabled(),
            Err("CollectionWorkflowDisabled")
        );
        assert!(CollectionWorkflowMode::Draining.observe_enabled());
        assert!(!CollectionWorkflowMode::Disabled.observe_enabled());
    }

    #[test]
    fn draining_and_disabled_reload_but_do_not_author_collections() {
        for mode in [
            CollectionWorkflowMode::Draining,
            CollectionWorkflowMode::Disabled,
        ] {
            assert!(mode.require_declaration_enabled(true).is_ok());
            assert!(mode.require_declaration_enabled(false).is_err());
        }
        assert!(
            CollectionWorkflowMode::Enabled
                .require_declaration_enabled(false)
                .is_ok()
        );
    }

    #[test]
    fn non_enabled_modes_require_the_complete_installed_spec_to_match() {
        const COLLECTION: &str = r#"
[automaton]
name = "Batch"
states = ["Draft"]
initial = "Draft"

[[collection_workflow]]
name = "items"
start_action = "Start"
cancel_action = "Cancel"
timeout_action = "Expire"
roster_field = "items"
member_entity = "Item"
member_action = "Start"
member_cancel_action = "Cancel"
max_members = 1
max_concurrency = 1
max_attempts = 1
on_success = "Succeeded"
on_partial_failure = "PartiallyFailed"
on_failure = "Failed"
on_cancelled = "Cancelled"
on_timed_out = "TimedOut"
"#;
        const ORDINARY: &str = r#"
[automaton]
name = "Batch"
states = ["Draft"]
initial = "Draft"
allow_indefinite_states = ["Draft"]
"#;
        for mode in [
            CollectionWorkflowMode::Draining,
            CollectionWorkflowMode::Disabled,
        ] {
            assert!(
                mode.require_spec_source(Some(COLLECTION), COLLECTION)
                    .is_ok()
            );
            let expected = match mode {
                CollectionWorkflowMode::Draining => "CollectionWorkflowDraining",
                CollectionWorkflowMode::Disabled => "CollectionWorkflowDisabled",
                CollectionWorkflowMode::Enabled => unreachable!(),
            };
            assert_eq!(
                mode.require_spec_source(Some("different"), COLLECTION),
                Err(expected.to_string())
            );
            assert_eq!(
                mode.require_spec_source(Some(COLLECTION), ORDINARY),
                Err(expected.to_string()),
                "non-enabled mode must not remove an installed collection declaration"
            );
            assert!(mode.require_spec_source(None, ORDINARY).is_ok());
        }
    }
}
