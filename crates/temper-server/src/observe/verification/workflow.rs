//! GET /observe/workflows -- full workflow view per app/tenant.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Json;
use serde::Serialize;
use temper_runtime::tenant::TenantId;
use tracing::instrument;

use crate::authz::{observe_tenant_scope, require_observe_auth};
use crate::registry::{SpecRegistry, VerificationStatus};
use crate::state::ServerState;

/// A single step in an entity's verification workflow.
#[derive(Serialize)]
struct WorkflowStep {
    step: String,
    status: String,
    passed: Option<bool>,
    timestamp: Option<String>,
    summary: Option<String>,
}

/// Per-entity workflow detail.
#[derive(Serialize)]
struct EntityWorkflow {
    entity_type: String,
    steps: Vec<WorkflowStep>,
}

/// Per-app/tenant workflow summary.
#[derive(Serialize)]
struct AppWorkflow {
    tenant: String,
    status: String,
    entities: Vec<EntityWorkflow>,
    runtime_events_count: u64,
}

/// Response for GET /observe/workflows.
#[derive(Serialize)]
pub(in crate::observe) struct WorkflowsResponse {
    workflows: Vec<AppWorkflow>,
}

/// Fetch design-time events from Postgres or Turso fallback.
async fn fetch_event_log(state: &ServerState) -> Vec<crate::state::DesignTimeEvent> {
    // Try Postgres first.
    if let Some(pool) = state
        .event_store
        .as_ref()
        .and_then(|store| store.postgres_pool())
    {
        type DtEventRow = (
            String,
            String,
            String,
            String,
            Option<String>,
            Option<bool>,
            Option<i16>,
            Option<i16>,
            chrono::DateTime<chrono::Utc>,
        );
        let rows: Result<Vec<DtEventRow>, sqlx::Error> = sqlx::query_as(
            "SELECT kind, entity_type, tenant, summary, level, passed, step_number, \
                        total_steps, created_at \
                 FROM design_time_events \
                 ORDER BY created_at ASC, id ASC",
        )
        .fetch_all(pool)
        .await;
        match rows {
            Ok(rows) => {
                return rows
                    .into_iter()
                    .map(
                        |(
                            kind,
                            entity_type,
                            tenant,
                            summary,
                            level,
                            passed,
                            step_number,
                            total_steps,
                            created_at,
                        )| crate::state::DesignTimeEvent {
                            kind,
                            entity_type,
                            tenant,
                            summary,
                            level,
                            passed,
                            timestamp: created_at.to_rfc3339(),
                            step_number: step_number.map(|n| n as u8),
                            total_steps: total_steps.map(|n| n as u8),
                        },
                    )
                    .collect();
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to read design_time_events from postgres");
            }
        }
    }

    // Turso fallback.
    if let Some(turso) = state.persistent_store() {
        match turso.list_design_time_events(None, 10_000).await {
            Ok(rows) => rows
                .into_iter()
                .map(|r| crate::state::DesignTimeEvent {
                    kind: r.kind,
                    entity_type: r.entity_type,
                    tenant: r.tenant,
                    summary: r.summary,
                    level: r.level,
                    passed: r.passed,
                    timestamp: r.created_at,
                    step_number: r.step_number.map(|n| n as u8),
                    total_steps: r.total_steps.map(|n| n as u8),
                })
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "failed to read design_time_events from Turso");
                Vec::new()
            }
        }
    } else {
        Vec::new()
    }
}

/// Fetch per-tenant trajectory counts from Turso.
async fn fetch_runtime_counts(state: &ServerState) -> std::collections::BTreeMap<String, u64> {
    let Some(turso) = state.persistent_store() else {
        return std::collections::BTreeMap::new();
    };
    match turso.load_recent_trajectories(100_000).await {
        Ok(rows) => {
            let mut counts = std::collections::BTreeMap::new();
            for row in &rows {
                *counts.entry(row.tenant.clone()).or_insert(0) += 1;
            }
            counts
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to read trajectory counts from Turso");
            std::collections::BTreeMap::new()
        }
    }
}

/// Build a step from an event log entry matching a given kind.
fn step_from_event(
    step_name: &str,
    events: &[&crate::state::DesignTimeEvent],
    kind: &str,
) -> WorkflowStep {
    let event = events.iter().find(|e| e.kind == kind);
    WorkflowStep {
        step: step_name.to_string(),
        status: if event.is_some() { "completed" } else { "pending" }.to_string(),
        passed: None,
        timestamp: event.map(|e| e.timestamp.clone()),
        summary: event.map(|e| e.summary.clone()),
    }
}

/// Build workflow steps for a single entity.
fn build_entity_workflow(
    registry: &SpecRegistry,
    tenant_id: &TenantId,
    entity_type: &str,
    entity_events: &[&crate::state::DesignTimeEvent],
    tenant_status: &mut &str,
) -> EntityWorkflow {
    let mut steps: Vec<WorkflowStep> = Vec::new();

    // Steps 1-2: loaded and verify_started
    let started_event = entity_events.iter().find(|e| e.kind == "verify_started");
    steps.push(step_from_event("loaded", entity_events, "spec_loaded"));
    steps.push(step_from_event("verify_started", entity_events, "verify_started"));

    // Steps 3-6: L0-L3 from verify_level events
    let level_events: Vec<_> = entity_events
        .iter()
        .filter(|e| e.kind == "verify_level")
        .collect();

    let level_labels = [
        "L0_symbolic",
        "L1_model_check",
        "L2_simulation",
        "L3_property_test",
    ];
    for (i, label) in level_labels.iter().enumerate() {
        let level_event = level_events.get(i);
        let status = match level_event {
            Some(_) => "completed",
            None => {
                if let Some(VerificationStatus::Running) =
                    registry.get_verification_status(tenant_id, entity_type)
                {
                    if (i == 0 && started_event.is_some() && level_events.is_empty())
                        || level_events.len() == i
                    {
                        "running"
                    } else {
                        "pending"
                    }
                } else {
                    "pending"
                }
            }
        };
        steps.push(WorkflowStep {
            step: label.to_string(),
            status: status.to_string(),
            passed: level_event.and_then(|e| e.passed),
            timestamp: level_event.map(|e| e.timestamp.clone()),
            summary: level_event.map(|e| e.summary.clone()),
        });
    }

    // Step 7: deployed
    let done_event = entity_events.iter().find(|e| e.kind == "verify_done");
    let deploy_status = match registry.get_verification_status(tenant_id, entity_type) {
        Some(VerificationStatus::Completed(result) | VerificationStatus::Restored(result)) => {
            if result.all_passed {
                "completed"
            } else {
                "failed"
            }
        }
        Some(VerificationStatus::Running) => {
            *tenant_status = "verifying";
            "running"
        }
        Some(VerificationStatus::Pending) | None => {
            if *tenant_status != "verifying" {
                *tenant_status = "loading";
            }
            "pending"
        }
    };
    steps.push(WorkflowStep {
        step: "deployed".to_string(),
        status: deploy_status.to_string(),
        passed: done_event.and_then(|e| e.passed),
        timestamp: done_event.map(|e| e.timestamp.clone()),
        summary: done_event.map(|e| e.summary.clone()).or_else(|| {
            if deploy_status == "completed" {
                Some("Entity ready for runtime".to_string())
            } else if deploy_status == "failed" {
                Some("Verification failed".to_string())
            } else {
                None
            }
        }),
    });

    if deploy_status == "failed" && *tenant_status != "verifying" {
        *tenant_status = "failed";
    }

    EntityWorkflow {
        entity_type: entity_type.to_string(),
        steps,
    }
}

/// GET /observe/workflows -- full workflow view per app/tenant.
///
/// Builds a Temporal-like workflow timeline from the design-time event log,
/// verification statuses, and trajectory log.
#[instrument(skip_all, fields(otel.name = "GET /observe/workflows"))]
pub(crate) async fn handle_workflows(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> Result<Json<WorkflowsResponse>, StatusCode> {
    require_observe_auth(&state, &headers, "read_events", "Event")?;
    let tenant_scope = observe_tenant_scope(&state, &headers)?;

    let event_log = fetch_event_log(&state).await;
    let runtime_counts = fetch_runtime_counts(&state).await;
    let registry = state.registry.read().unwrap(); // ci-ok: infallible lock

    let mut workflows = Vec::new();
    for tenant_id in registry.tenant_ids() {
        if let Some(ref scope) = tenant_scope
            && tenant_id != scope
        {
            continue;
        }
        let tenant_str = tenant_id.as_str().to_string();
        if tenant_str == "system" {
            continue;
        }

        let mut tenant_status = "completed";
        let entity_workflows: Vec<EntityWorkflow> = registry
            .entity_types(tenant_id)
            .into_iter()
            .map(|entity_type| {
                let entity_events: Vec<_> = event_log
                    .iter()
                    .filter(|e| e.tenant == tenant_str && e.entity_type == entity_type)
                    .collect();
                build_entity_workflow(
                    &registry,
                    tenant_id,
                    entity_type,
                    &entity_events,
                    &mut tenant_status,
                )
            })
            .collect();

        let runtime_count = *runtime_counts.get(&tenant_str).unwrap_or(&0);
        workflows.push(AppWorkflow {
            tenant: tenant_str,
            status: tenant_status.to_string(),
            entities: entity_workflows,
            runtime_events_count: runtime_count,
        });
    }

    Ok(Json(WorkflowsResponse { workflows }))
}
