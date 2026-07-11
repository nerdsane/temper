//! Compensating dispatch for background integration failures (ADR-0152).
//!
//! When a background integration fails with no declared `on_failure`, the
//! source action's transition is already durable, so a rollback is impossible.
//! Instead we dispatch a **compensating transition** — a forward step that moves
//! the source entity into a failure state — so the failure becomes an observable
//! state change rather than a silently dropped error.
//!
//! If the source entity's spec declares no enabled `Fail`/error transition from
//! its current state, the failure is surfaced as a critical metric plus an
//! Observe event — never a silent drop.
//!
//! Determinism: the compensation dispatch routes through `dispatch_tenant_action`
//! and the candidate-transition lookup walks the spec's `TransitionTable`
//! deterministically (`BTreeMap` rule index). The *decision to compensate* runs
//! inside the existing `// determinism-ok` background spawn, outside the
//! simulation core; what is deterministic is the chosen action and its dispatch.

use temper_runtime::tenant::TenantId;
use tracing::Instrument;

use crate::request_context::AgentContext;

/// Candidate names for a declared failure/error transition, in priority order.
///
/// A spec opts into compensation by declaring one of these as an action on the
/// source entity. `Fail` is the canonical name; the others let existing specs
/// that already model an error step participate without renaming.
const FAILURE_TRANSITION_NAMES: &[&str] = &["Fail", "MarkFailed", "Error", "Abort"];

impl crate::state::ServerState {
    /// Dispatch a compensating transition for a background integration failure
    /// (ADR-0152).
    ///
    /// `triggering_action` is the source action whose integration failed. This
    /// is a **sync** method — like `dispatch_spawn_requests` — that performs the
    /// async compensation inside a `tokio::spawn`, so the compensating dispatch
    /// future is not part of the integration-dispatch future (which would form
    /// an async-recursion type cycle, since the compensating action can itself
    /// trigger integrations).
    ///
    /// The spawned task resolves the source entity's current status, finds the
    /// first declared failure transition (see [`FAILURE_TRANSITION_NAMES`])
    /// enabled from that status, and dispatches it. If none is enabled, it emits
    /// a surfaced critical metric and an `integration_failure_dropped` Observe
    /// event — never a silent drop.
    pub(crate) fn dispatch_integration_failure_compensation(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        triggering_action: &str,
        error: &str,
    ) {
        let state = self.clone();
        let tenant = tenant.clone();
        let entity_type = entity_type.to_string();
        let entity_id = entity_id.to_string();
        let triggering_action = triggering_action.to_string();
        let error = error.to_string();
        let span = tracing::info_span!(
            "dispatch.integration_failure_compensation",
            tenant = %tenant,
            entity_type = %entity_type,
            entity_id = %entity_id,
            trigger_action = %triggering_action,
        );

        tokio::spawn(
            async move {
                // determinism-ok: compensation is a background side-effect that
                // runs outside the simulation core (matches the spawn boundary
                // of the integration dispatch it compensates for).
                state
                    .run_integration_failure_compensation(
                        &tenant,
                        &entity_type,
                        &entity_id,
                        &triggering_action,
                        &error,
                    )
                    .await;
            }
            .instrument(span),
        );
    }

    /// Async body of [`dispatch_integration_failure_compensation`], run inside a
    /// spawned task.
    async fn run_integration_failure_compensation(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        triggering_action: &str,
        error: &str,
    ) {
        let status = match self
            .resolve_entity_status(tenant, entity_type, entity_id)
            .await
        {
            Ok(status) => status.unwrap_or_default(),
            Err(status_error) => {
                tracing::error!(
                    tenant = %tenant,
                    entity_type,
                    entity_id,
                    error = %status_error,
                    "failed to resolve entity status for integration compensation"
                );
                String::new()
            }
        };

        let compensating_action =
            self.find_failure_transition(tenant, entity_type, status.as_str());

        let Some(action) = compensating_action else {
            // No failure path declared: surface, never drop.
            self.surface_dropped_integration_failure(
                tenant,
                entity_type,
                entity_id,
                triggering_action,
                status.as_str(),
                error,
                "no on_failure and no enabled Fail/error transition",
            );
            return;
        };

        let params = serde_json::json!({
            "error": error,
            "error_message": error,
            "trigger_action": triggering_action,
        });
        let agent_ctx = AgentContext::for_service("integration-compensation");
        match self
            .dispatch_tenant_action(tenant, entity_type, entity_id, &action, params, &agent_ctx)
            .await
        {
            Ok(resp) if resp.success => {
                tracing::info!(
                    tenant = %tenant,
                    entity_type,
                    entity_id,
                    trigger_action = triggering_action,
                    compensating_action = %action,
                    "dispatched compensating failure transition for background integration failure"
                );
            }
            // The compensation was dispatched but rejected (e.g. the entity
            // left the eligible state in a race), or the dispatch itself errored.
            // Either way the failure was not turned into a state change: surface
            // it, never drop.
            Ok(resp) => {
                self.surface_dropped_integration_failure(
                    tenant,
                    entity_type,
                    entity_id,
                    triggering_action,
                    status.as_str(),
                    resp.error.as_deref().unwrap_or("compensation rejected"),
                    "compensating transition was rejected",
                );
            }
            Err(e) => {
                self.surface_dropped_integration_failure(
                    tenant,
                    entity_type,
                    entity_id,
                    triggering_action,
                    status.as_str(),
                    &e,
                    "compensating transition dispatch failed",
                );
            }
        }
    }

    /// Emit the critical metric + Observe event for a background integration
    /// failure that could not be compensated (ADR-0152). Never silent.
    #[allow(clippy::too_many_arguments)]
    fn surface_dropped_integration_failure(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        triggering_action: &str,
        status: &str,
        error: &str,
        reason: &str,
    ) {
        crate::runtime_metrics::record_integration_failure_dropped(
            tenant.as_str(),
            entity_type,
            triggering_action,
            status,
        );
        let seq = self.next_entity_event_sequence(tenant.as_str(), entity_type, entity_id);
        self.record_entity_observe_event_with_seq(
            tenant.as_str(),
            entity_type,
            entity_id,
            seq,
            "integration_failure_dropped",
            serde_json::json!({
                "seq": seq,
                "trigger_action": triggering_action,
                "state": status,
                "error": error,
                "reason": reason,
            }),
        );
        tracing::error!(
            tenant = %tenant,
            entity_type,
            entity_id,
            trigger_action = triggering_action,
            state = %status,
            error,
            reason,
            "background integration failure could not be compensated \u{2014} surfaced, not dropped"
        );
    }

    /// Find the first declared failure/error transition enabled from `status`.
    ///
    /// Walks the source entity's `TransitionTable` deterministically and returns
    /// the action name of the highest-priority [`FAILURE_TRANSITION_NAMES`] entry
    /// that has a rule firing from `status`. Returns `None` when the spec
    /// declares no such transition.
    fn find_failure_transition(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        status: &str,
    ) -> Option<String> {
        let registry = self.registry.read().unwrap(); // ci-ok: infallible lock
        let spec = registry.get_spec(tenant, entity_type)?;
        let table = spec.table();

        for candidate in FAILURE_TRANSITION_NAMES {
            let matching = table
                .rules
                .iter()
                .find(|rule| rule.name.eq_ignore_ascii_case(candidate));
            if let Some(rule) = matching
                && (rule.from_states.is_empty() || rule.from_states.iter().any(|s| s == status))
            {
                // Return the spec's actual action name (preserve its casing).
                return Some(rule.name.clone());
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use crate::registry::SpecRegistry;
    use crate::state::ServerState;
    use temper_runtime::ActorSystem;
    use temper_runtime::tenant::TenantId;
    use temper_spec::csdl::parse_csdl;

    const CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.CompensationTest" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Job">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Jobs" EntityType="Temper.CompensationTest.Job"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

    const JOB_WITH_FAIL: &str = r#"
[automaton]
name = "Job"
states = ["Running", "Done", "Failed"]
initial = "Running"

[[action]]
name = "Complete"
from = ["Running"]
to = "Done"

[[action]]
name = "Fail"
from = ["Running"]
to = "Failed"
"#;

    const JOB_NO_FAIL: &str = r#"
[automaton]
name = "Job"
states = ["Running", "Done"]
initial = "Running"

[[action]]
name = "Complete"
from = ["Running"]
to = "Done"
"#;

    fn state_with(job_ioa: &str) -> ServerState {
        let csdl = parse_csdl(CSDL).expect("CSDL parses");
        let mut registry = SpecRegistry::new();
        registry.register_tenant("default", csdl, CSDL.to_string(), &[("Job", job_ioa)]);
        ServerState::from_registry(ActorSystem::new("compensation-test"), registry)
    }

    #[test]
    fn finds_fail_transition_enabled_from_current_state() {
        let state = state_with(JOB_WITH_FAIL);
        let tenant = TenantId::default();
        assert_eq!(
            state.find_failure_transition(&tenant, "Job", "Running"),
            Some("Fail".to_string())
        );
    }

    #[test]
    fn no_fail_transition_when_not_enabled_from_state() {
        let state = state_with(JOB_WITH_FAIL);
        let tenant = TenantId::default();
        // Fail only fires from Running; from Done there is no failure path.
        assert_eq!(state.find_failure_transition(&tenant, "Job", "Done"), None);
    }

    #[test]
    fn no_fail_transition_when_spec_declares_none() {
        let state = state_with(JOB_NO_FAIL);
        let tenant = TenantId::default();
        assert_eq!(
            state.find_failure_transition(&tenant, "Job", "Running"),
            None
        );
    }
}
