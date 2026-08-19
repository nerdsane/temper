//! Server state shared across all request handlers.

pub(crate) mod account_verification;
pub mod admission;
pub(crate) mod app_uniqueness;
mod construct;
pub mod custom_effects;
mod dispatch;
mod entity_ops;
mod evolution;
mod file_initial_writes;
mod file_read_blobs;
mod file_read_projection;
mod file_reads;
mod file_writes;
pub mod metrics;
mod observe_events;
mod parity;
pub mod pending_decisions;
mod persistence;
pub mod policy_suggestions;
mod projection_backfill;
mod published_artifacts;
mod query_projection_queue;
pub(crate) mod rate_limit;
mod runtime_metrics;
pub(crate) mod storage_caps;
mod stores;
mod stores_metadata;
pub mod trajectory;
pub mod wasm_invocation_log;

pub use admission::{AdmissionController, AdmissionOutcome, AdmissionPermit};
pub(crate) use dispatch::authorized_http_endpoint_host;
#[cfg(feature = "observe")]
pub(crate) use dispatch::internal_http_capability_issuer;
pub use dispatch::{DispatchCommand, DispatchError, DispatchExtOptions, StateTimeoutTracker};
pub use entity_ops::{FailedLevelInfo, VerificationGateError};
#[cfg(feature = "observe")]
pub(crate) use file_reads::{BatchTextReadError, validate_batch_text_ids};
pub use file_reads::{IndexedFileStreamRead, TextFileReadResult, TextFileVersionReadResult};
pub(crate) use file_writes::FileStreamContentError;
pub use metrics::MetricsCollector;
pub use observe_events::{
    AgentProgressEvent, DesignTimeEvent, EntityObserveEvent, ObserveRefreshHint,
};
pub use parity::{QueryProjectionReplayParityDrift, QueryProjectionReplayParityReport};
pub use pending_decisions::{
    ActionScope, DecisionStatus, DurationScope, PendingDecision, PolicyScopeMatrix, PrincipalScope,
    ResourceScope,
};
pub use persistence::WasmModuleSource;
pub use policy_suggestions::PolicySuggestionEngine;
pub use published_artifacts::PublishFileArtifactRequest;
#[cfg(feature = "observe")]
pub(crate) use published_artifacts::{
    PUBLISH_ARTIFACT_STALE_AUTHORIZATION, PublishArtifactAuthorization,
};
pub(crate) use query_projection_queue::{ProjectionEnqueueOutcome, QueryProjectionWriteQueue};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;
use temper_actor_runtime::ActorSystem as PgActorSystem;
use temper_authz::AuthzEngine;
use temper_evolution::PostgresRecordStore;
#[allow(deprecated)]
// ADR-0025 Phase 4: remove after sentinel/insight dispatch migrated to IOA entities
use temper_evolution::store::RecordStore;
use temper_jit::table::TransitionTable;
use temper_runtime::ActorSystem;
use temper_runtime::actor::ActorRef;
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::CsdlDocument;
pub use trajectory::{TrajectoryEntry, TrajectorySource};
pub use wasm_invocation_log::WasmInvocationEntry;

use crate::adapters::AdapterRegistry;
use crate::entity_actor::{EntityMsg, SnapshotWriteQueue};
use crate::events::EntityStateChange;
use crate::idempotency::IdempotencyCache;
use crate::internal_invocation::InternalInvocationCredentialStore;
use crate::ots_trajectory_outbox::OtsTrajectoryOutbox;
use crate::registry::SpecRegistry;
use crate::secrets::vault::SecretsVault;
use crate::storage::StorageStack;
use crate::trigger::ReactionDispatcher;
use crate::wasm_registry::WasmModuleRegistry;
use crate::webhooks::WebhookDispatcher;
use temper_wasm::WasmEngine;

/// Platform extension point invoked after a governed OData bound action
/// succeeds. The hook is deliberately post-dispatch and action-scoped: specs
/// still define the action, authorize it, and produce any declared writes.
pub struct BoundActionHookContext<'a> {
    pub state: &'a ServerState,
    pub tenant: &'a TenantId,
    pub entity_type: &'a str,
    pub entity_id: &'a str,
    pub action: &'a str,
    pub params: &'a serde_json::Value,
    pub state_json: &'a serde_json::Value,
}

#[async_trait::async_trait]
pub trait BoundActionHook: Send + Sync {
    async fn after_bound_action(
        &self,
        ctx: BoundActionHookContext<'_>,
    ) -> Result<Option<serde_json::Value>, String>;
}

pub(crate) fn env_bool(name: &str, default: bool) -> bool {
    let val = std::env::var(name); // determinism-ok: read once at startup
    match val {
        Ok(v) => match v.trim().to_ascii_lowercase().as_str() {
            "0" | "false" | "off" | "no" => false,
            "1" | "true" | "on" | "yes" => true,
            _ => default,
        },
        Err(_) => default,
    }
}

pub(crate) fn env_timeout() -> Duration {
    let secs: u64 = std::env::var("TEMPER_ACTION_TIMEOUT_SECS") // determinism-ok: read once at startup
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(5);
    debug_assert!(secs > 0 && secs <= 300, "action timeout must be 1-300s");
    Duration::from_secs(secs)
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name) // determinism-ok: read once at startup
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

pub(crate) fn normalize_local_tdata_host(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }

    let hostish = trimmed
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(trimmed)
        .split('/')
        .next()
        .unwrap_or("")
        .trim();
    if hostish.is_empty() {
        return None;
    }

    let host = if let Some(rest) = hostish.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        hostish.split(':').next().unwrap_or("")
    }
    .trim()
    .trim_matches('.')
    .to_ascii_lowercase();

    if host.is_empty() || host.chars().any(char::is_whitespace) {
        return None;
    }

    Some(host)
}

pub(crate) fn env_local_tdata_hosts() -> BTreeSet<String> {
    let mut hosts = BTreeSet::new();
    let configured_hosts = std::env::var("TEMPER_LOCAL_TDATA_HOSTS"); // determinism-ok: read once at startup
    if let Ok(raw) = configured_hosts {
        for item in raw.split(',') {
            if let Some(host) = normalize_local_tdata_host(item) {
                hosts.insert(host);
            }
        }
    }

    for name in [
        "TEMPER_PUBLIC_BASE_URL",
        "PUBLIC_BASE_URL",
        "RAILWAY_PUBLIC_DOMAIN",
        "RAILWAY_STATIC_URL",
    ] {
        let raw = std::env::var(name); // determinism-ok: read once at startup
        if let Ok(raw) = raw
            && let Some(host) = normalize_local_tdata_host(&raw)
        {
            hosts.insert(host);
        }
    }

    hosts
}

pub(crate) fn state_cache_budget() -> usize {
    static STATE_CACHE_BUDGET: OnceLock<usize> = OnceLock::new();
    *STATE_CACHE_BUDGET.get_or_init(|| env_usize("TEMPER_STATE_CACHE_BUDGET", 10_000))
}

#[derive(Clone)]
// ADR-0025 Phase 4: remove record_store field after IOA entity migration complete
pub struct ServerState {
    /// Capture losses this server could not record against any session.
    ///
    /// Read by conformance checking: a non-zero count means some stored
    /// session is missing rows and nothing durable says which, so no report
    /// from this server can claim to have seen a whole run.
    pub(crate) capture_health: crate::trajectory_outbox::CaptureHealth,

    /// The actor system for spawning and managing legacy in-memory entity actors.
    pub actor_system: Arc<ActorSystem>,
    /// Optional PG-backed actor system. When configured and an entity type is in
    /// actor_backed_types, OData reads/writes dispatch through this runtime.
    pub pg_actor_system: Option<Arc<PgActorSystem>>,
    /// Entity types backed by pg_actor_system.
    ///
    /// Entries may be global entity type names (for example, `Order`) or
    /// tenant-scoped keys (`tenant:Order`) for canarying one tenant without
    /// changing same-named entity types in other tenants.
    pub actor_backed_types: BTreeSet<String>,
    /// Parsed CSDL document describing the entity model (legacy single-tenant).
    pub csdl: Arc<CsdlDocument>,
    /// Raw CSDL XML string for serving via `$metadata` (legacy single-tenant).
    pub csdl_xml: Arc<String>,
    /// Maps entity set names to entity type names (legacy single-tenant).
    pub entity_set_map: Arc<BTreeMap<String, String>>,
    /// Transition table per entity type (legacy single-tenant).
    pub transition_tables: Arc<BTreeMap<String, Arc<TransitionTable>>>,
    /// Live actor registry: actor_key -> ActorRef.
    pub actor_registry: Arc<RwLock<BTreeMap<String, ActorRef<EntityMsg>>>>,
    /// Last access time per actor key (used for idle passivation).
    pub last_accessed: Arc<RwLock<BTreeMap<String, chrono::DateTime<chrono::Utc>>>>,
    /// First-class storage capabilities selected for this runtime.
    pub storage_stack: Option<Arc<StorageStack>>,
    /// Bounded background writer for derived query-plane projection rows.
    pub(crate) query_projection_queue: Arc<Mutex<Option<Arc<QueryProjectionWriteQueue>>>>,
    /// Bounded background writer for durable actor recovery snapshots.
    pub(crate) snapshot_write_queue: Arc<Mutex<Option<Arc<SnapshotWriteQueue>>>>,
    /// Bounded background writer for full OTS trajectory artifacts.
    pub(crate) ots_trajectory_outbox: Arc<Mutex<Option<Arc<OtsTrajectoryOutbox>>>>,
    /// Runtime data directory for persisted local metadata (e.g. specs registry).
    pub data_dir: std::path::PathBuf,
    /// Agent hints learned from trajectory analysis, keyed by action name.
    pub agent_hints: Arc<RwLock<BTreeMap<TenantId, BTreeMap<String, String>>>>,
    /// Cedar ABAC authorization engine.
    pub authz: Arc<AuthzEngine>,
    /// Multi-tenant specification registry (shared, mutable for live registration).
    pub registry: Arc<RwLock<SpecRegistry>>,
    /// Index of entity IDs per (tenant:entity_type) for collection queries.
    pub entity_index: Arc<RwLock<BTreeMap<String, BTreeSet<String>>>>,
    /// `{tenant}:{entity_type}` keys whose `entity_index` entry has been fully
    /// hydrated from the durable event store. A type is only complete once a
    /// store scan has run for it; lazily spawning a single actor must NOT mark
    /// it complete, or a partial index can hide durable entities from
    /// collection queries (a consumer would read present, durable entities as
    /// "not found").
    pub entity_index_hydrated: Arc<RwLock<BTreeSet<String>>>,
    /// `{tenant}:{entity_type}` keys whose `entity_key_index` backfill is complete
    /// `"tenant:entity_type" -> covered key-set` (ADR-0153 watermark cache). The value
    /// is the sorted comma-joined declared key names the backfill covered. A keyed read
    /// MISS on that type is authoritative absence ONLY when the covered key-set still
    /// equals the currently-declared one — so a newly-declared, not-yet-backfilled key
    /// never reads a present entity as absent (it falls back to the scan). This turns a
    /// 413-scan into an O(log n) answer once the type is fully keyed (ARN-68). Loaded
    /// lazily from the durable watermark (see [`key_index_watermarks_loaded`]).
    pub key_index_backfilled: Arc<RwLock<BTreeMap<String, String>>>,
    /// Tenants whose durable watermarks have been read into `key_index_backfilled`
    /// at least once this run. Gates the one-time-per-tenant load on the read path.
    pub key_index_watermarks_loaded: Arc<RwLock<BTreeSet<String>>>,
    /// Broadcast channel for entity state change events (SSE subscriptions).
    pub event_tx: Arc<tokio::sync::broadcast::Sender<EntityStateChange>>,
    /// Broadcast channel for replayable per-entity lifecycle and progress events.
    pub entity_observe_tx: Arc<tokio::sync::broadcast::Sender<EntityObserveEvent>>,
    /// Server start time (DST-safe: uses sim_now()).
    pub start_time: chrono::DateTime<chrono::Utc>,
    /// Metrics collector for the /observe endpoints.
    pub metrics: Arc<MetricsCollector>,
    /// In-memory evolution record store (O/P/A/D/I records).
    #[allow(deprecated)] // ADR-0025 Phase 4: remove after chain validation replaced
    pub record_store: Arc<RecordStore>,
    /// Optional Postgres evolution record store (source of truth when configured).
    pub pg_record_store: Option<Arc<PostgresRecordStore>>,
    /// Optional reaction dispatcher for cross-entity coordination.
    ///
    /// Wrapped in `RwLock` so hot-loaded specs can refresh reaction rules at runtime.
    pub reaction_dispatcher: Arc<RwLock<Option<Arc<ReactionDispatcher>>>>,
    /// Optional webhook dispatcher for external system notifications.
    pub webhook_dispatcher: Option<Arc<WebhookDispatcher>>,
    /// Native adapter integration registry (`type = "adapter"` dispatch path).
    pub adapter_registry: Arc<AdapterRegistry>,
    /// WASM module registry: maps (tenant, module_name) → sha256_hash.
    pub wasm_module_registry: Arc<RwLock<WasmModuleRegistry>>,
    /// WASM execution engine: compiles, caches, and invokes sandboxed modules.
    pub wasm_engine: Arc<WasmEngine>,
    /// Global cross-entity invariant enforcement toggle.
    pub cross_invariant_enforce: bool,
    /// Whether eventual invariants should block writes.
    pub cross_invariant_eventual_enforce: bool,
    /// Broadcast channel for design-time events (spec loading, verification progress).
    pub design_time_tx: Arc<tokio::sync::broadcast::Sender<DesignTimeEvent>>,
    /// LRU cache of entity current state, updated on every state change broadcast.
    /// Key: "{tenant}:{entity_type}:{entity_id}", Value: (current_state, last_updated).
    /// Capped at [`state_cache_budget()`] entries; oldest entry evicted automatically.
    #[allow(clippy::type_complexity)]
    pub entity_state_cache:
        Arc<Mutex<lru::LruCache<String, (String, chrono::DateTime<chrono::Utc>)>>>,
    /// Configurable timeout for actor ask operations (default: 5s).
    pub action_dispatch_timeout: Duration,
    /// Admission control (ADR-0051). Gates concurrent action dispatches per
    /// `(tenant, entity_type, action)` based on caps declared in entity
    /// specs' `[admission]` blocks.
    pub admission: Arc<AdmissionController>,
    /// State-timeout arm-sequence tracker (ADR-0049). Per-entity in-memory
    /// counter used to cancel stale timers when the entity transitions out
    /// of a declared state or reset_on fires.
    pub state_timeout_tracker: Arc<StateTimeoutTracker>,
    /// Eventual invariant convergence tracker.
    pub eventual_tracker: Arc<RwLock<crate::eventual_invariants::EventualInvariantTracker>>,
    /// Idempotency cache for deduplicating agent retries.
    pub idempotency_cache: Arc<IdempotencyCache>,
    /// Bounded, single-use credentials for authenticated internal HTTP re-entry.
    pub internal_invocation_credentials: InternalInvocationCredentialStore,
    /// Optional encrypted secrets vault for per-tenant secret management.
    /// Broadcast channel for new pending decisions (SSE subscriptions).
    pub pending_decision_tx: Arc<tokio::sync::broadcast::Sender<PendingDecision>>,
    /// Per-tenant Cedar policy text (tenant -> policy text).
    pub tenant_policies: Arc<RwLock<BTreeMap<String, String>>>,
    /// Serializes approval commit/activation so concurrent approvals cannot
    /// replace one another with policy text derived from a stale cache.
    #[cfg(feature = "observe")]
    pub(crate) policy_approval_lock: Arc<tokio::sync::Mutex<()>>,
    /// Tenants installed in commons mode. Collection creates for these tenants
    /// must pass Cedar so commons guardrail forbids apply to direct OData
    /// writes as well as bound actions and composite sub-writes.
    pub commons_guardrail_tenants: Arc<RwLock<BTreeSet<String>>>,
    /// In-process token buckets keyed by `(tenant, owner, action_class)`.
    ///
    /// Buckets hydrate from RateLimit entities on first use, then mirror token
    /// consumption back to the entity log.
    pub(crate) commons_rate_limit_buckets:
        Arc<Mutex<BTreeMap<String, rate_limit::RuntimeRateLimitBucket>>>,
    /// Cached per-owner storage projections keyed by `(tenant, owner)`.
    ///
    /// The cache is intentionally invalidated broadly on Owner/Repository/Blob
    /// writes; storage cap enforcement prefers freshness over narrow retention.
    pub(crate) commons_storage_projection_cache:
        Arc<Mutex<BTreeMap<String, storage_caps::CommonsStorageProjection>>>,
    /// Pending owner-byte reservations held by admitted raw Blob uploads.
    commons_storage_reservations:
        Arc<Mutex<BTreeMap<String, storage_caps::CommonsStorageReservationEntry>>>,
    /// Coarse commons-mode write guardrail lock.
    ///
    /// Held from preflight through persistence for commons writes so exact
    /// guardrails such as storage caps and App name uniqueness cannot race
    /// between "check" and "write" while cross-actor transactions are still
    /// being built out.
    pub(crate) commons_write_guardrail_lock: Arc<tokio::sync::Mutex<()>>,
    /// Weighted declared-byte admission for disk-backed raw Blob ingest.
    /// State ownership permits deterministic capacity injection in simulation.
    pub(crate) raw_blob_ingest_budget: crate::blob_store::BlobIngestBudget,
    pub secrets_vault: Option<Arc<SecretsVault>>,
    /// Broadcast channel for agent progress events (SSE subscriptions).
    /// // determinism-ok: broadcast channel for external observation only
    pub agent_progress_tx: Arc<tokio::sync::broadcast::Sender<AgentProgressEvent>>,
    /// Monotonic per-entity observe-event sequence counters.
    pub entity_event_sequences: Arc<Mutex<BTreeMap<String, u64>>>,
    /// Replay buffer for recent per-entity observe events.
    pub entity_observe_log: Arc<Mutex<BTreeMap<String, Vec<EntityObserveEvent>>>>,
    /// Broadcast channel for observe UI refresh hints (SSE push).
    /// // determinism-ok: broadcast channel for external observation only
    pub observe_refresh_tx: Arc<tokio::sync::broadcast::Sender<ObserveRefreshHint>>,
    /// Listening port for HTTP REPL self-referencing calls.
    pub listen_port: Arc<std::sync::OnceLock<u16>>,
    /// When true, missing `X-Tenant-Id` headers fall back to the first
    /// registered tenant (legacy single-tenant compat).  When false
    /// (multi-tenant mode), a missing header is rejected with 400.
    pub single_tenant_mode: bool,
    /// Denial pattern detection engine for Cedar policy suggestions.
    pub suggestion_engine: Arc<RwLock<PolicySuggestionEngine>>,
    /// When set, spec verification runs in an isolated child process.
    ///
    /// Points to the `temper` binary that supports the `verify-ioa` subcommand.
    /// Each entity's IOA source is written to stdin; the result is read from stdout
    /// as JSON. A 30-second timeout is applied per entity.
    pub verify_subprocess_bin: Option<Arc<std::path::PathBuf>>,
    /// Optional custom effect handler for platform-level hooks.
    ///
    /// When set, the post-dispatch pipeline calls this handler for each
    /// custom effect triggered by entity transitions. This is the extension
    /// point that `temper-platform` uses to wire `hooks.rs` into the
    /// dispatch pipeline.
    pub custom_effect_handler: Option<Arc<dyn custom_effects::CustomEffectHandler>>,
    /// Optional hook for platform-owned post-action work such as installing a
    /// Genesis app after the spec-owned `App.Install` action succeeds.
    pub bound_action_hook: Option<Arc<dyn BoundActionHook>>,
    /// Per-tenant HttpEndpoint route tables (ADR-0069 Phase 2).
    /// Consulted by the router fallback to dispatch to WASM
    /// integrations registered via the HttpEndpoint entity.
    pub http_endpoint_tables: Arc<crate::http_endpoint::HttpEndpointTables>,
    /// Shared HTTP stream registry for ADR-0057 streaming exchanges.
    /// Held by ServerState so the dispatcher can mint inbound
    /// exchanges before handing the guest-facing handles to the
    /// WASM invocation. Per-request ProductionWasmHost instances
    /// receive a clone of this Arc via `with_shared_streams` so
    /// FFI calls from the guest resolve to the same handle IDs.
    pub http_stream_registry: Arc<temper_wasm::http_stream::HttpStreamRegistry>,
    /// Long-lived workflow root spans keyed by workflow.run_id.
    pub(crate) workflow_spans: Arc<crate::workflow_tracing::WorkflowSpanRegistry>,
    /// Public hostnames owned by this process that may use in-process TData
    /// dispatch from WASM guests instead of leaving through the public edge.
    pub(crate) local_tdata_hosts: Arc<BTreeSet<String>>,
}

/// Install a one-time hook so liveness violations surfaced by temper-spec
/// emit OTel counters (ADR-0050). Idempotent — subsequent calls are no-ops.
pub(crate) fn install_liveness_metrics_reporter_once() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        temper_spec::automaton::set_liveness_violation_reporter(|v| {
            crate::runtime_metrics::record_spec_liveness_violation(&v.entity, &v.state);
        });
    });
}

#[cfg(test)]
mod tests {
    use super::normalize_local_tdata_host;

    #[test]
    fn normalize_local_tdata_host_accepts_urls_domains_and_ports() {
        assert_eq!(
            normalize_local_tdata_host("https://Temper.Example/tdata"),
            Some("temper.example".to_string())
        );
        assert_eq!(
            normalize_local_tdata_host("openpaw-production.up.railway.app"),
            Some("openpaw-production.up.railway.app".to_string())
        );
        assert_eq!(
            normalize_local_tdata_host("http://127.0.0.1:8080"),
            Some("127.0.0.1".to_string())
        );
    }

    #[test]
    fn normalize_local_tdata_host_rejects_empty_or_whitespace_hosts() {
        assert_eq!(normalize_local_tdata_host(""), None);
        assert_eq!(normalize_local_tdata_host("https:///tdata"), None);
        assert_eq!(normalize_local_tdata_host("bad host.example"), None);
    }
}
