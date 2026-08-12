//! Trajectory analysis endpoints: conformance checking and ATIF export.
//!
//! Both return one named run's recorded content — its prompts, its decisions,
//! its tool results, its request bodies — so both are gated by
//! [`require_trajectory_content_auth`]: a Cedar permit for `read_trajectories`
//! on `Trajectory` in the addressed tenant, with no principal-kind bypass. The
//! aggregate `/observe/trajectories` view lets an Admin principal past Cedar
//! without a policy, and the principal kind is a request header the platform
//! does not yet authenticate; these two endpoints do not inherit that.
//!
//! Unlike that aggregate view, these two address one tenant's data by
//! identifier — a session id, a trajectory id — so a cross-tenant admin view
//! has nothing to resolve against. Both require an explicit `X-Tenant-Id` and
//! answer `400` without one.
//!
//! That gate is the strictest one the platform currently has, and it is not a
//! sufficient one: the principal it evaluates is read off unauthenticated
//! request headers. See [`require_trajectory_content_auth`] for what remains
//! open, ARN-255 for the platform-wide fix, and ARN-187 for the gate over the
//! pre-existing OTS upload and list routes.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Json;
use serde::Deserialize;
use temper_ots::AtifTrajectory;
use temper_ots::models::OTSTrajectory;
use temper_runtime::tenant::TenantId;
use temper_spec::automaton::Automaton;
use tracing::instrument;

use crate::authz::{observe_tenant_scope, require_trajectory_content_auth};
use crate::conformance::{ConformanceInput, SpecResolution, check_conformance};
use crate::state::ServerState;

/// Largest session the conformance checker will read in one request.
///
/// The checker holds the whole session in memory and the state-machine walk is
/// order-dependent, so it cannot be paged; the cap bounds the read instead.
const MAX_CONFORMANCE_ROWS: i64 = 5_000;

/// Denial body for both endpoints.
///
/// Names the permit an operator has to install, because the gate has no
/// principal-kind bypass and a bare "unauthorized" leaves them guessing which
/// action and resource Cedar was asked about.
const UNAUTHORIZED_DETAIL: &str = "unauthorized: this endpoint returns recorded agent content and \
     requires a Cedar permit for action `read_trajectories` on resource `Trajectory` in the \
     addressed tenant";

/// Body of `POST /api/conformance/check`.
#[derive(Debug, Deserialize)]
pub(crate) struct ConformanceCheckRequest {
    /// The actor spec to check the run against.
    entity_type: String,
    /// The session whose rows form the run.
    session_id: String,
    /// Optional OTS trajectory contributing the agent-side decisions.
    #[serde(default)]
    trajectory_id: Option<String>,
    /// The spec version the run executed under, for a run whose trajectory
    /// does not carry one.
    ///
    /// A hint, never an override: when the trajectory pins a version, that one
    /// governs and a conflicting hint is refused. See
    /// [`resolve_governing_spec`].
    #[serde(default)]
    spec_version: Option<String>,
    /// Cap on rows read, bounded by [`MAX_CONFORMANCE_ROWS`].
    #[serde(default)]
    limit: Option<i64>,
}

/// POST /api/conformance/check — check one session against its actor spec.
#[instrument(skip_all, fields(otel.name = "POST /api/conformance/check"))]
pub(crate) async fn handle_conformance_check(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(request): Json<ConformanceCheckRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let tenant = required_tenant(&state, &headers)?;
    require_trajectory_content_auth(
        &state,
        &headers,
        "read_trajectories",
        "Trajectory",
        tenant.as_str(),
    )
    .map_err(|status| (status, UNAUTHORIZED_DETAIL.to_string()))?;

    let limit = match request.limit {
        Some(limit) if !(1..=MAX_CONFORMANCE_ROWS).contains(&limit) => {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("limit must be between 1 and {MAX_CONFORMANCE_ROWS}"),
            ));
        }
        Some(limit) => limit,
        None => MAX_CONFORMANCE_ROWS,
    };

    let spec = registered_spec(&state, &tenant, &request.entity_type)?;

    let Some(store) = state.metadata_store_for_tenant(tenant.as_str()).await else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "no durable metadata backend configured; conformance needs stored trajectories"
                .to_string(),
        ));
    };

    // Entity type is deliberately not pushed into the query: the checker
    // reports rows from other actors as skipped, which is how a caller learns
    // a session touched entities this spec does not govern.
    //
    // One row past the cap is read so a session that ends exactly on the cap
    // is reported complete instead of partial. The extra row is dropped before
    // the walk, which must see a prefix and nothing beyond it.
    let mut rows = store
        .query_trajectories_by_session(
            &request.session_id,
            Some(tenant.as_str()),
            None,
            limit.saturating_add(1),
        )
        .await
        .map_err(|error| {
            tracing::warn!(error = %error, "failed to read session trajectories");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to read session trajectories: {error}"),
            )
        })?;
    let truncated = rows.len() as i64 > limit;
    rows.truncate(limit as usize);

    let ots = match request.trajectory_id.as_deref() {
        Some(trajectory_id) => {
            let (ots_session_id, trajectory) =
                load_ots_trajectory(&store, &tenant, trajectory_id).await?;
            // Folding another run's decisions into this session's report would
            // produce violations that belong to neither run.
            if !ots_session_id.is_empty() && ots_session_id != request.session_id {
                return Err((
                    StatusCode::BAD_REQUEST,
                    format!(
                        "trajectory `{trajectory_id}` belongs to session `{ots_session_id}`, \
                         not `{}`",
                        request.session_id
                    ),
                ));
            }
            Some(trajectory)
        }
        None => None,
    };

    let spec_resolution = resolve_governing_spec(
        &spec,
        ots.as_ref()
            .and_then(|ots| ots.metadata.spec_version.as_deref()),
        request.spec_version.as_deref(),
        &request.entity_type,
    )?;

    let report = check_conformance(ConformanceInput {
        automaton: &spec.automaton,
        kernel_rows: &rows,
        ots_trajectory: ots.as_ref(),
        rows_truncated: truncated,
        spec_resolution,
    });
    tracing::info!(
        tenant = %tenant,
        entity_type = %request.entity_type,
        session_id = %request.session_id,
        spec_version = %spec.version,
        spec_resolution = ?spec_resolution,
        rows = rows.len(),
        truncated,
        verdict = ?report.verdict,
        passed = report.passed,
        evidence_complete = report.evidence_complete,
        violations = report.violations.len(),
        "conformance.check"
    );

    Ok(Json(serde_json::json!({
        "tenant": tenant.as_str(),
        "entity_type": request.entity_type,
        "session_id": request.session_id,
        "trajectory_id": request.trajectory_id,
        // The spec the run was judged against, so a report can never be read
        // without knowing which version produced it.
        "spec_version": spec.version,
        "row_limit": limit,
        "truncated": truncated,
        "report": report,
    })))
}

/// GET /api/ots/trajectories/{id}/atif — export one trajectory as ATIF v1.7.
#[instrument(skip_all, fields(otel.name = "GET /api/ots/trajectories/{id}/atif"))]
pub(crate) async fn handle_get_ots_trajectory_atif(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path(trajectory_id): Path<String>,
) -> Result<Json<AtifTrajectory>, (StatusCode, String)> {
    let tenant = required_tenant(&state, &headers)?;
    require_trajectory_content_auth(
        &state,
        &headers,
        "read_trajectories",
        "Trajectory",
        tenant.as_str(),
    )
    .map_err(|status| (status, UNAUTHORIZED_DETAIL.to_string()))?;

    let Some(store) = state.metadata_store_for_tenant(tenant.as_str()).await else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "no durable metadata backend configured; nothing to export".to_string(),
        ));
    };

    let (session_id, trajectory) = load_ots_trajectory(&store, &tenant, &trajectory_id).await?;
    // The session lives on the storage row, not in the OTS document; an empty
    // column means the upload carried no session header, so ATIF omits it
    // rather than reporting an empty run identity.
    let session_id = (!session_id.is_empty()).then_some(session_id);
    let atif = temper_ots::to_atif(&trajectory, session_id.as_deref()).map_err(|error| {
        // The stored trajectory is intact; it just has no valid ATIF
        // rendering, which is the caller's answer rather than a server fault.
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("trajectory `{trajectory_id}` cannot be exported as ATIF: {error}"),
        )
    })?;
    Ok(Json(atif))
}

/// Resolve the tenant, requiring an explicit one.
fn required_tenant(
    state: &ServerState,
    headers: &HeaderMap,
) -> Result<TenantId, (StatusCode, String)> {
    let scope = observe_tenant_scope(state, headers).map_err(|status| {
        (
            status,
            "unauthorized: no tenant scope could be resolved for this request".to_string(),
        )
    })?;
    scope.ok_or((
        StatusCode::BAD_REQUEST,
        "X-Tenant-Id is required: this endpoint addresses one tenant's trajectories".to_string(),
    ))
}

/// A registered spec and the identity of the source it was parsed from.
struct RegisteredSpec {
    automaton: Automaton,
    /// Content hash of the IOA source, the same identity the spec store keeps
    /// and the value a harness records as `metadata.spec_version`.
    version: String,
}

/// Load the actor spec for `entity_type`, cloned out of the registry lock.
fn registered_spec(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
) -> Result<RegisteredSpec, (StatusCode, String)> {
    let registry = state.registry.read().map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "spec registry lock poisoned".to_string(),
        )
    })?;
    registry
        .get_spec(tenant, entity_type)
        .map(|spec| RegisteredSpec {
            automaton: spec.automaton.clone(),
            version: temper_store_turso::spec_content_hash(&spec.ioa_source),
        })
        .ok_or((
            StatusCode::NOT_FOUND,
            format!("no spec registered for entity type `{entity_type}` in tenant `{tenant}`"),
        ))
}

/// Refuse to judge a run by a spec that did not govern it.
///
/// A conformance report only means something against the spec the run executed
/// under. Temper keeps one spec per (tenant, entity type) — a submit replaces
/// the previous version rather than versioning it — so the registered spec is
/// whatever was submitted last, which need not be the one in force when the run
/// happened. Three cases follow.
///
/// **The trajectory pins a version.** `OTSMetadata.spec_version` is written by
/// the harness that drove the run, from inside the run; it is the provenance.
/// It governs. A request that names a different version is refused rather than
/// honoured: the request body is the one part of this input a caller chooses
/// freely, and letting it override the recorded provenance would turn "check
/// this run against its spec" into "check this run against whichever spec makes
/// it pass".
///
/// **Only the request names a version.** Nothing recorded contradicts it, so it
/// is used — and, like a pinned version, it has to match the registered spec or
/// the check has nothing to run against.
///
/// **A named version is not the registered one.** The governing spec is gone,
/// so there is nothing to check against. A report against the current spec
/// would pass behaviour that violated the old one and condemn behaviour that
/// was legal under it, so the request is refused.
///
/// **Nothing names a version.** The check runs against the registered spec and
/// says so: the resolution is [`SpecResolution::Unresolved`], which the report
/// carries as an evidence gap, so the run cannot come back `passed`.
fn resolve_governing_spec(
    spec: &RegisteredSpec,
    pinned_spec_version: Option<&str>,
    requested_spec_version: Option<&str>,
    entity_type: &str,
) -> Result<SpecResolution, (StatusCode, String)> {
    let governing = match (pinned_spec_version, requested_spec_version) {
        (Some(pinned), Some(requested)) if !names_same_spec(pinned, requested) => {
            return Err((
                StatusCode::CONFLICT,
                format!(
                    "the request asks to check `{entity_type}` against spec version \
                     `{requested}`, but the trajectory records that the run executed under \
                     `{pinned}`; the recorded version is the run's provenance and a request \
                     cannot override it"
                ),
            ));
        }
        (Some(pinned), _) => pinned,
        (None, Some(requested)) => requested,
        (None, None) => return Ok(SpecResolution::Unresolved),
    };
    if names_same_spec(governing, &spec.version) {
        return Ok(SpecResolution::Pinned);
    }
    Err((
        StatusCode::CONFLICT,
        format!(
            "the run executed under spec version `{governing}` for `{entity_type}`, but the \
             registered spec is `{}`; Temper stores one version per entity type, so the governing \
             spec is not available to check against and a report against the current one would \
             judge the run by rules it never ran under",
            spec.version
        ),
    ))
}

/// Whether two recorded spec versions name the same spec.
///
/// A harness may record the content hash bare or `sha256:`-prefixed; both name
/// the same spec, and a string comparison alone would read them as a conflict.
fn names_same_spec(left: &str, right: &str) -> bool {
    fn bare(version: &str) -> &str {
        version.strip_prefix("sha256:").unwrap_or(version)
    }
    bare(left) == bare(right)
}

/// Load and parse a stored OTS trajectory, returning its session id with it.
///
/// A missing row is a `404`; stored data that no longer parses is a `500` and
/// is logged, because that is a storage fault rather than a bad request.
async fn load_ots_trajectory(
    store: &std::sync::Arc<dyn crate::storage::MetadataStore>,
    tenant: &TenantId,
    trajectory_id: &str,
) -> Result<(String, OTSTrajectory), (StatusCode, String)> {
    let document = store
        .get_ots_trajectory(tenant.as_str(), trajectory_id)
        .await
        .map_err(|error| {
            tracing::warn!(error = %error, "failed to read OTS trajectory");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to read OTS trajectory: {error}"),
            )
        })?
        .ok_or((
            StatusCode::NOT_FOUND,
            format!("no OTS trajectory `{trajectory_id}` in tenant `{tenant}`"),
        ))?;

    let trajectory: OTSTrajectory = serde_json::from_str(&document.data).map_err(|error| {
        tracing::error!(
            error = %error,
            trajectory_id = %trajectory_id,
            "stored OTS trajectory does not parse"
        );
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("stored OTS trajectory `{trajectory_id}` does not parse: {error}"),
        )
    })?;
    Ok((document.session_id, trajectory))
}

#[cfg(test)]
#[path = "trajectory_analysis_test.rs"]
mod trajectory_analysis_test;
