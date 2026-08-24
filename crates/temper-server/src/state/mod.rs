//! Server state shared across all request handlers.

pub(crate) mod account_verification;
pub mod admission;
pub(crate) mod app_uniqueness;
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
pub mod pending_decisions;
mod persistence;
pub mod policy_suggestions;
mod projection_backfill;
mod published_artifacts;
mod query_projection_queue;
pub(crate) mod rate_limit;
mod reference_contract;
mod runtime_metrics;
mod schema_pin;
pub(crate) use schema_pin::SCHEMA_PIN_MISMATCH_PREFIX;
pub(crate) mod storage_caps;
pub mod trajectory;
pub mod wasm_invocation_log;

pub use admission::{AdmissionController, AdmissionOutcome, AdmissionPermit};
pub(crate) use dispatch::authorized_http_endpoint_host;
#[cfg(feature = "observe")]
pub(crate) use dispatch::internal_http_capability_issuer;
pub use dispatch::{DispatchCommand, DispatchError, DispatchExtOptions, StateTimeoutTracker};
pub(crate) use entity_ops::validate_global_entity_id;
pub use entity_ops::{FailedLevelInfo, VerificationGateError};
#[cfg(feature = "observe")]
pub(crate) use file_reads::{BatchTextReadError, validate_batch_text_ids};
pub use file_reads::{IndexedFileStreamRead, TextFileReadResult, TextFileVersionReadResult};
pub(crate) use file_writes::FileStreamContentError;
pub use metrics::MetricsCollector;
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
pub use trajectory::{TrajectoryEntry, TrajectorySource};
pub use wasm_invocation_log::WasmInvocationEntry;

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;
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
use temper_runtime::persistence::schema_deployment::{SchemaMigrationJob, SchemaMigrationStatus};
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::CsdlDocument;
use temper_store_postgres::PostgresEventStore;

use crate::adapters::AdapterRegistry;
use crate::entity_actor::{EntityMsg, SnapshotWriteQueue};
use crate::events::EntityStateChange;
use crate::idempotency::IdempotencyCache;
use crate::internal_invocation::InternalInvocationCredentialStore;
use crate::ots_trajectory_outbox::OtsTrajectoryOutbox;
use crate::registry::SpecRegistry;
use crate::secrets::vault::SecretsVault;
use crate::storage::{
    BackendLabel, BoxedEventStore, DataOnlyCreateStore, MetadataStore, PolicyStore,
    QueryPlaneStore, SchemaDeploymentStoreDyn, StorageStack, TrajectorySink,
};
use crate::trigger::ReactionDispatcher;
use crate::wasm_registry::WasmModuleRegistry;
use crate::webhooks::WebhookDispatcher;
use temper_wasm::{StreamRegistry, WasmEngine, WasmInvocationContext};

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

/// An agent progress event for remote observation via SSE.
///
/// These events are broadcast so that the executor (or any observer) can
/// track agent activity in real time without polling.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentProgressEvent {
    /// Tenant that owns the related entity.
    pub tenant: String,
    /// Entity type that emitted the event.
    pub entity_type: String,
    /// Entity ID that emitted the event.
    pub entity_id: String,
    /// Monotonic per-entity event sequence.
    pub seq: u64,
    /// Event kind: "tool_call_started", "tool_call_completed",
    /// "task_started", "task_completed", "agent_completed".
    pub kind: String,
    /// The agent ID this event relates to.
    pub agent_id: String,
    /// Optional tool call ID (for tool_call_* events).
    pub tool_call_id: Option<String>,
    /// Optional tool name (for tool_call_* events).
    pub tool_name: Option<String>,
    /// Optional task ID (for task_* events).
    pub task_id: Option<String>,
    /// Optional result or status message.
    pub message: Option<String>,
    /// ISO-8601 timestamp when the event was created.
    pub timestamp: String,
    /// Optional structured payload.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// Unified replayable event stream for a single entity.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EntityObserveEvent {
    /// Tenant that owns the entity.
    pub tenant: String,
    /// Entity type for this event.
    pub entity_type: String,
    /// Entity instance ID.
    pub entity_id: String,
    /// Monotonic per-entity event sequence.
    pub seq: u64,
    /// SSE event name.
    pub event_name: String,
    /// Structured event payload.
    pub data: serde_json::Value,
}

/// Lightweight hint broadcast for the Observe UI SSE refresh stream.
///
/// Each variant signals that a particular domain's data has changed.
/// The frontend subscribes to `/observe/refresh/stream` and re-fetches
/// the relevant REST endpoint when it receives a matching hint.
#[derive(Clone, Debug, serde::Serialize)]
pub enum ObserveRefreshHint {
    Specs,
    Entities,
    Verification,
    Trajectories,
    Agents,
    Policies,
    EvolutionRecords,
    EvolutionInsights,
    UnmetIntents,
    FeatureRequests,
    OsApps,
    Decisions,
}

/// A design-time event emitted during spec loading and verification.
///
/// These events are broadcast via SSE so the observe UI can show
/// verification progress in real time (design-time observation).
#[derive(Debug, Clone, serde::Serialize)]
pub struct DesignTimeEvent {
    /// Event kind: "spec_loaded", "verify_started", "verify_level", "verify_done".
    pub kind: String,
    /// Entity type this event relates to.
    pub entity_type: String,
    /// Tenant this event relates to.
    pub tenant: String,
    /// Human-readable summary.
    pub summary: String,
    /// Verification level name (for "verify_level" events).
    pub level: Option<String>,
    /// Whether this level/entity passed (for "verify_level" and "verify_done" events).
    pub passed: Option<bool>,
    /// ISO-8601 timestamp when the event was created.
    pub timestamp: String,
    /// Step number in the workflow (1=loaded, 2=verify_started, 3-6=L0-L3, 7=done).
    pub step_number: Option<u8>,
    /// Total steps in the workflow (always 7 for verification).
    pub total_steps: Option<u8>,
}

fn env_bool(name: &str, default: bool) -> bool {
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

fn env_timeout() -> Duration {
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

fn normalize_local_tdata_host(raw: &str) -> Option<String> {
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

fn env_local_tdata_hosts() -> BTreeSet<String> {
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

/// Tenants that have opted into exporting raw LLM content to telemetry, loaded
/// once at startup from `TEMPER_LLM_CONTENT_EXPORT_TENANTS` (comma-separated
/// tenant ids; the literal `*` opts in every tenant). Empty by default, so
/// content is redacted for every tenant unless explicitly opted in. See
/// ADR-0166.
fn env_llm_content_export_tenants() -> BTreeSet<String> {
    let configured = std::env::var("TEMPER_LLM_CONTENT_EXPORT_TENANTS"); // determinism-ok: read once at startup
    parse_llm_content_export_tenants(configured.as_deref().unwrap_or(""))
}

/// Parse the opt-in list. Split out from the env read so the policy is testable
/// without mutating process environment (which races across parallel tests).
fn parse_llm_content_export_tenants(raw: &str) -> BTreeSet<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

/// Whether `tenant` is opted into raw LLM content export. Redact-by-default: an
/// empty set exports for nobody. Split out from [`ServerState::export_llm_content`]
/// so the decision can be tested without building a whole server state.
fn tenant_exports_llm_content(opted_in: &BTreeSet<String>, tenant: &str) -> bool {
    opted_in.contains("*") || opted_in.contains(tenant)
}

fn state_cache_budget() -> usize {
    static STATE_CACHE_BUDGET: OnceLock<usize> = OnceLock::new();
    *STATE_CACHE_BUDGET.get_or_init(|| env_usize("TEMPER_STATE_CACHE_BUDGET", 10_000))
}

/// Summary of a query-projection replay parity verification run.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct QueryProjectionReplayParityReport {
    /// Tenant whose persisted journals and projection rows were compared.
    pub tenant: String,
    /// Optional entity type scope applied to this verifier run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity_type: Option<String>,
    /// Optional maximum number of entities considered by this verifier run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity_limit: Option<u64>,
    /// Number of persisted entities considered by the verifier.
    pub checked: u64,
    /// Number of non-deleted entities whose projection row matched replayed state.
    pub matched: u64,
    /// Number of entities whose projection row diverged from replayed state.
    pub drifted: u64,
    /// Number of active entities missing from the projection catalog.
    pub missing: u64,
    /// Number of replayed deleted entities correctly absent from the catalog.
    pub deleted_absent: u64,
    /// Number of entities the verifier could not compare because of a store or spec error.
    pub errors: u64,
    /// Bounded sample of drift/error examples for operator diagnosis.
    pub drift_examples: Vec<QueryProjectionReplayParityDrift>,
}

impl QueryProjectionReplayParityReport {
    /// Returns true when every checked entity matched the projection contract.
    pub fn is_clean(&self) -> bool {
        self.drifted == 0 && self.missing == 0 && self.errors == 0
    }
}

/// One bounded replay parity drift or error example.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct QueryProjectionReplayParityDrift {
    /// Entity type whose projection diverged from replayed state.
    pub entity_type: String,
    /// Entity identifier for targeted repair. This is intentionally not emitted as a metric tag.
    pub entity_id: String,
    /// Drift class, such as `fields`, `sequence`, `missing_catalog`, or `deleted_present`.
    pub drift_kind: String,
    /// Sequence relationship between catalog and replayed state.
    pub sequence_direction: String,
    /// Absolute sequence gap when both sides have sequence numbers.
    pub sequence_gap: u64,
    /// Sequence number currently stored in the projection catalog, when a row exists.
    pub catalog_sequence: Option<u64>,
    /// Sequence number recovered from authoritative event replay.
    pub authoritative_sequence: u64,
}

/// Shared state for the Temper HTTP server.
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
    /// Generation used to retire recovery workers after a dispatcher rebuild.
    reaction_recovery_generation: Arc<std::sync::atomic::AtomicU64>,
    /// Lifetime sentinel held by server-owned state clones, but not recovery workers.
    reaction_recovery_owner: Arc<()>,
    /// Bounded handoff to the durable schema-migration supervisor.
    schema_migration_supervisor_tx:
        Arc<Mutex<Option<tokio::sync::mpsc::Sender<SchemaMigrationJob>>>>,
    /// Generation used to retire replaced schema-migration supervisors.
    schema_migration_supervisor_generation: Arc<std::sync::atomic::AtomicU64>,
    /// Lifetime sentinel held by server-owned state clones, not the worker clone.
    schema_migration_supervisor_owner: Arc<()>,
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
    /// Tenants that have opted into exporting raw LLM content (prompts,
    /// completions, system instructions, tool arguments/results) to the
    /// telemetry backend. Loaded once from `TEMPER_LLM_CONTENT_EXPORT_TENANTS`.
    /// Empty by default: every tenant is redacted unless explicitly opted in.
    /// See ADR-0166.
    pub(crate) llm_content_export_tenants: Arc<BTreeSet<String>>,
}

/// Install a one-time hook so liveness violations surfaced by temper-spec
/// emit OTel counters (ADR-0050). Idempotent — subsequent calls are no-ops.
fn install_liveness_metrics_reporter_once() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        temper_spec::automaton::set_liveness_violation_reporter(|v| {
            crate::runtime_metrics::record_spec_liveness_violation(&v.entity, &v.state);
        });
    });
}

#[allow(deprecated)] // ADR-0025 Phase 4: RecordStore used until chain validation replaced
impl ServerState {
    /// Enable commons-mode write guardrails for a tenant.
    pub fn enable_commons_guardrails(&self, tenant: &str) {
        if let Ok(mut tenants) = self.commons_guardrail_tenants.write() {
            tenants.insert(tenant.to_string());
        }
    }

    /// Whether `tenant` may export raw LLM content (prompts, completions,
    /// system instructions, tool arguments/results) to the telemetry backend.
    ///
    /// Redact-by-default: content is only exported for tenants listed in
    /// `TEMPER_LLM_CONTENT_EXPORT_TENANTS` (or when it contains the wildcard
    /// `*`). Every other tenant is redacted. See ADR-0166.
    pub fn export_llm_content(&self, tenant: &str) -> bool {
        tenant_exports_llm_content(&self.llm_content_export_tenants, tenant)
    }

    /// Whether commons-mode write guardrails are active for a tenant.
    pub fn commons_guardrails_enabled(&self, tenant: &TenantId) -> bool {
        self.commons_guardrail_tenants
            .read()
            .map(|tenants| tenants.contains(tenant.as_str()))
            .unwrap_or(false)
    }

    /// Attach the composed runtime storage stack.
    pub fn set_storage_stack(&mut self, stack: StorageStack) {
        let stack = Arc::new(stack);
        let snapshot_queue = SnapshotWriteQueue::start(stack.events.clone());
        if let Ok(mut slot) = self.snapshot_write_queue.lock() {
            *slot = Some(snapshot_queue);
        }
        if let Some(query_plane) = stack.query_plane.clone() {
            let queue = QueryProjectionWriteQueue::start(query_plane);
            if let Ok(mut slot) = self.query_projection_queue.lock() {
                *slot = Some(queue);
            }
        }
        if let Some(metadata) = stack.metadata.clone() {
            let queue = OtsTrajectoryOutbox::start();
            queue.recover_queued_metadata_stores(stack.backend.as_str(), metadata);
            if let Ok(mut slot) = self.ots_trajectory_outbox.lock() {
                *slot = Some(queue);
            }
        }
        self.storage_stack = Some(stack);
        self.spawn_schema_migration_supervisor();
        let dispatcher = self
            .reaction_dispatcher
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .unwrap_or_else(|| {
                let registry = self
                    .registry
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .build_reaction_registry();
                Arc::new(ReactionDispatcher::new(Arc::new(registry)))
            });
        if let Ok(mut slot) = self.reaction_dispatcher.write() {
            *slot = Some(Arc::clone(&dispatcher));
        }
        self.spawn_reaction_recovery(dispatcher);
    }

    /// Hand one already-claimed migration to the bounded local supervisor.
    pub(crate) fn enqueue_schema_migration(&self, job: SchemaMigrationJob) -> Result<(), String> {
        let sender = self
            .schema_migration_supervisor_tx
            .lock()
            .map_err(|_| "schema migration supervisor lock poisoned".to_string())?
            .clone()
            .ok_or_else(|| "schema migration supervisor is unavailable".to_string())?;
        sender.try_send(job).map_err(|error| match error {
            tokio::sync::mpsc::error::TrySendError::Full(_) => {
                "schema migration supervisor queue budget exhausted".to_string()
            }
            tokio::sync::mpsc::error::TrySendError::Closed(_) => {
                "schema migration supervisor stopped".to_string()
            }
        })
    }

    fn spawn_schema_migration_supervisor(&self) {
        if self
            .storage_stack
            .as_ref()
            .and_then(|stack| stack.schema_deployments.as_ref())
            .is_none()
            || tokio::runtime::Handle::try_current().is_err()
        {
            return;
        }
        let (sender, mut receiver) = tokio::sync::mpsc::channel(128);
        if let Ok(mut slot) = self.schema_migration_supervisor_tx.lock() {
            *slot = Some(sender);
        } else {
            return;
        }
        let generation = self
            .schema_migration_supervisor_generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            .saturating_add(1);
        let owner = Arc::downgrade(&self.schema_migration_supervisor_owner);
        let mut state = self.clone();
        state.schema_migration_supervisor_owner = Arc::new(());
        tokio::spawn(async move {
            // determinism-ok: production durable-work supervisor; simulation
            // tests drive the same fenced batch method explicitly.
            let mut scan = tokio::time::interval(std::time::Duration::from_secs(1));
            scan.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                if owner.upgrade().is_none()
                    || state
                        .schema_migration_supervisor_generation
                        .load(std::sync::atomic::Ordering::SeqCst)
                        != generation
                {
                    return;
                }
                let direct = tokio::select! { // determinism-ok: production durable-work scheduling
                    job = receiver.recv() => job,
                    _ = scan.tick() => None,
                };
                let jobs = match direct {
                    Some(job) => vec![job],
                    None => {
                        let Some(store) = state
                            .storage_stack
                            .as_ref()
                            .and_then(|stack| stack.schema_deployments.as_ref())
                        else {
                            return;
                        };
                        match store.list_incomplete_schema_migrations(128).await {
                            Ok(jobs) => {
                                let now = u64::try_from(sim_now().timestamp_millis()).unwrap_or(0);
                                jobs.into_iter()
                                    .filter(|job| {
                                        job.status != SchemaMigrationStatus::Migrating
                                            || job
                                                .lease_expires_at
                                                .is_some_and(|deadline| deadline <= now)
                                    })
                                    .collect()
                            }
                            Err(error) => {
                                tracing::error!(%error, "schema migration recovery scan failed");
                                Vec::new()
                            }
                        }
                    }
                };
                for job in jobs {
                    let job_id = job.command.job_id.clone();
                    if let Err(error) =
                        crate::schema_deployment::GovernedSchemaDeploymentService::new(&state)
                            .drive_migration(job)
                            .await
                    {
                        tracing::error!(job_id, error = %error.message(), "schema migration supervisor cycle failed");
                    }
                }
            }
        });
    }

    /// Return the durable query-plane capability for projection reads/writes.
    pub(crate) fn query_plane_store(&self) -> Option<Arc<dyn QueryPlaneStore>> {
        self.storage_stack
            .as_ref()
            .and_then(|stack| stack.query_plane.clone())
    }

    /// Return the native data-only create capability when the backend supports it.
    pub(crate) fn data_only_create_store(&self) -> Option<Arc<dyn DataOnlyCreateStore>> {
        self.storage_stack
            .as_ref()
            .and_then(|stack| stack.data_only_create.clone())
    }

    /// Return the runtime event journal capability plus backend label.
    pub(crate) fn event_journal(&self) -> Option<(BoxedEventStore, BackendLabel)> {
        self.storage_stack
            .as_ref()
            .map(|stack| (stack.events.clone(), stack.backend))
    }

    /// Return the durable schema-deployment capability when configured.
    pub(crate) fn schema_deployment_store(&self) -> Option<Arc<dyn SchemaDeploymentStoreDyn>> {
        self.storage_stack
            .as_ref()
            .and_then(|stack| stack.schema_deployments.clone())
    }

    /// Return the granular Cedar policy persistence capability.
    pub fn policy_store(&self) -> Option<Arc<dyn PolicyStore>> {
        self.storage_stack
            .as_ref()
            .and_then(|stack| stack.policies.clone())
    }

    /// Return the observe trajectory sink plus backend label for metrics.
    pub(crate) fn trajectory_sink(&self) -> Option<(&'static str, Arc<dyn TrajectorySink>)> {
        if let Some(stack) = self.storage_stack.as_ref()
            && let Some(sink) = stack.trajectory.clone()
        {
            return Some((stack.backend.as_str(), sink));
        }

        None
    }

    /// Return the OTS trajectory artifact outbox plus backend label.
    #[cfg_attr(not(feature = "observe"), allow(dead_code))]
    pub(crate) fn ots_trajectory_outbox(&self) -> Option<(&'static str, Arc<OtsTrajectoryOutbox>)> {
        let backend = self.storage_stack.as_ref()?.backend.as_str();
        let queue = self.ots_trajectory_outbox.lock().ok()?.clone()?;
        Some((backend, queue))
    }

    /// Create ServerState from CSDL XML and optional specification sources.
    pub fn new(system: ActorSystem, csdl: CsdlDocument, csdl_xml: String) -> Self {
        install_liveness_metrics_reporter_once();
        let mut entity_set_map = BTreeMap::new();
        for schema in &csdl.schemas {
            for container in &schema.entity_containers {
                for entity_set in &container.entity_sets {
                    let type_name = entity_set
                        .entity_type
                        .rsplit('.')
                        .next()
                        .unwrap_or(&entity_set.entity_type);
                    entity_set_map.insert(entity_set.name.clone(), type_name.to_string());
                }
            }
        }

        let (event_tx, _) = tokio::sync::broadcast::channel(256); // determinism-ok: broadcast for external observation
        let (entity_observe_tx, _) = tokio::sync::broadcast::channel(512); // determinism-ok: broadcast for external observation
        let (design_time_tx, _) = tokio::sync::broadcast::channel(256); // determinism-ok: broadcast for external observation
        let (pending_decision_tx, _) = tokio::sync::broadcast::channel(256); // determinism-ok: broadcast for external observation
        let (agent_progress_tx, _) = tokio::sync::broadcast::channel(256); // determinism-ok: broadcast for external observation
        let (observe_refresh_tx, _) = tokio::sync::broadcast::channel(64); // determinism-ok: broadcast for external observation
        let state = Self {
            capture_health: crate::trajectory_outbox::CaptureHealth::default(),
            actor_system: Arc::new(system),
            pg_actor_system: None,
            actor_backed_types: BTreeSet::new(),
            csdl: Arc::new(csdl),
            csdl_xml: Arc::new(csdl_xml),
            entity_set_map: Arc::new(entity_set_map),
            transition_tables: Arc::new(BTreeMap::new()),
            actor_registry: Arc::new(RwLock::new(BTreeMap::new())),
            last_accessed: Arc::new(RwLock::new(BTreeMap::new())),
            storage_stack: None,
            query_projection_queue: Arc::new(Mutex::new(None)),
            snapshot_write_queue: Arc::new(Mutex::new(None)),
            ots_trajectory_outbox: Arc::new(Mutex::new(None)),
            data_dir: std::path::PathBuf::new(),
            agent_hints: Arc::new(RwLock::new(BTreeMap::new())),
            // Network and tenant-scoped authorization starts fail-closed.
            // Tests/development that intentionally need a permissive tenant
            // must install that tenant policy explicitly (ARN-230).
            authz: Arc::new(AuthzEngine::empty()),
            registry: Arc::new(RwLock::new(SpecRegistry::new())),
            entity_index: Arc::new(RwLock::new(BTreeMap::new())),
            entity_index_hydrated: Arc::new(RwLock::new(BTreeSet::new())),
            key_index_backfilled: Arc::new(RwLock::new(BTreeMap::new())),
            key_index_watermarks_loaded: Arc::new(RwLock::new(BTreeSet::new())),
            event_tx: Arc::new(event_tx),
            entity_observe_tx: Arc::new(entity_observe_tx),
            start_time: sim_now(),
            metrics: Arc::new(MetricsCollector::new()),
            record_store: Arc::new(RecordStore::new()),
            pg_record_store: None,
            reaction_dispatcher: Arc::new(RwLock::new(None)),
            reaction_recovery_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            reaction_recovery_owner: Arc::new(()),
            schema_migration_supervisor_tx: Arc::new(Mutex::new(None)),
            schema_migration_supervisor_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            schema_migration_supervisor_owner: Arc::new(()),
            webhook_dispatcher: None,
            adapter_registry: Arc::new(AdapterRegistry::with_builtins()),
            wasm_module_registry: Arc::new(RwLock::new(WasmModuleRegistry::new())),
            wasm_engine: Arc::new(WasmEngine::default()),
            cross_invariant_enforce: env_bool("TEMPER_XINV_ENFORCE", true),
            cross_invariant_eventual_enforce: env_bool("TEMPER_XINV_EVENTUAL_ENFORCE", true),
            design_time_tx: Arc::new(design_time_tx),
            entity_state_cache: Arc::new(Mutex::new(lru::LruCache::new(
                NonZeroUsize::new(state_cache_budget()).expect("cache budget must be > 0"),
            ))),
            action_dispatch_timeout: env_timeout(),
            admission: Arc::new(AdmissionController::new()),
            state_timeout_tracker: Arc::new(StateTimeoutTracker::new()),
            eventual_tracker: Arc::new(RwLock::new(
                crate::eventual_invariants::EventualInvariantTracker::new(),
            )),
            idempotency_cache: Arc::new(IdempotencyCache::new()),
            internal_invocation_credentials: InternalInvocationCredentialStore::runtime(),
            pending_decision_tx: Arc::new(pending_decision_tx),
            tenant_policies: Arc::new(RwLock::new(BTreeMap::new())),
            #[cfg(feature = "observe")]
            policy_approval_lock: Arc::new(tokio::sync::Mutex::new(())),
            commons_guardrail_tenants: Arc::new(RwLock::new(BTreeSet::new())),
            commons_rate_limit_buckets: Arc::new(Mutex::new(BTreeMap::new())),
            commons_storage_projection_cache: Arc::new(Mutex::new(BTreeMap::new())),
            commons_storage_reservations: Arc::new(Mutex::new(BTreeMap::new())),
            commons_write_guardrail_lock: Arc::new(tokio::sync::Mutex::new(())),
            raw_blob_ingest_budget: crate::blob_store::BlobIngestBudget::runtime(),
            secrets_vault: None,
            agent_progress_tx: Arc::new(agent_progress_tx), // determinism-ok: broadcast for external observation
            entity_event_sequences: Arc::new(Mutex::new(BTreeMap::new())),
            entity_observe_log: Arc::new(Mutex::new(BTreeMap::new())),
            observe_refresh_tx: Arc::new(observe_refresh_tx), // determinism-ok: broadcast for external observation
            listen_port: Arc::new(std::sync::OnceLock::new()),
            single_tenant_mode: true,
            suggestion_engine: Arc::new(RwLock::new(PolicySuggestionEngine::new())),
            verify_subprocess_bin: None,
            custom_effect_handler: None,
            bound_action_hook: None,
            http_endpoint_tables: Arc::new(crate::http_endpoint::HttpEndpointTables::new()),
            http_stream_registry: Arc::new(temper_wasm::http_stream::HttpStreamRegistry::new()),
            workflow_spans: Arc::new(crate::workflow_tracing::WorkflowSpanRegistry::default()),
            local_tdata_hosts: Arc::new(env_local_tdata_hosts()),
            llm_content_export_tenants: Arc::new(env_llm_content_export_tenants()),
        };

        // Pre-register built-in WASM modules (http_fetch for generic HTTP integrations).
        state.register_builtin_wasm_modules();
        state
    }

    /// Compile and register built-in WASM modules (e.g. http_fetch).
    fn register_builtin_wasm_modules(&self) {
        /// Embedded http_fetch WASM binary, compiled from wasm-modules/http-fetch.
        const HTTP_FETCH_WASM: &[u8] =
            include_bytes!("../../../temper-wasm/modules/http_fetch.wasm");

        match self.wasm_engine.compile_and_cache(HTTP_FETCH_WASM) {
            Ok(hash) => {
                if let Ok(mut registry) = self.wasm_module_registry.write() {
                    registry.register_builtin("http_fetch", &hash);
                    tracing::info!(module = "http_fetch", hash = %hash, "registered built-in WASM module");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to compile built-in http_fetch WASM module");
            }
        }
    }

    fn push_entity_observe_event(&self, event: EntityObserveEvent) {
        let key = format!("{}:{}:{}", event.tenant, event.entity_type, event.entity_id);
        {
            let mut log = self.entity_observe_log.lock().unwrap(); // ci-ok: infallible lock
            let entries = log.entry(key).or_default();
            entries.push(event.clone());
            if entries.len() > 512 {
                let overflow = entries.len().saturating_sub(512);
                entries.drain(0..overflow);
            }
        }
        let _ = self.entity_observe_tx.send(event);
    }

    pub(crate) fn next_entity_event_sequence(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
    ) -> u64 {
        let key = format!("{tenant}:{entity_type}:{entity_id}");
        let mut sequences = self.entity_event_sequences.lock().unwrap(); // ci-ok: infallible lock
        let next = sequences.get(&key).copied().unwrap_or(0) + 1;
        sequences.insert(key, next);
        next
    }

    pub(crate) fn record_entity_observe_event_with_seq(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        seq: u64,
        event_name: &str,
        data: serde_json::Value,
    ) {
        let event = EntityObserveEvent {
            tenant: tenant.to_string(),
            entity_type: entity_type.to_string(),
            entity_id: entity_id.to_string(),
            seq,
            event_name: event_name.to_string(),
            data,
        };
        self.push_entity_observe_event(event);
    }

    #[cfg(feature = "observe")]
    pub(crate) fn replay_entity_observe_events(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        since: u64,
    ) -> Vec<EntityObserveEvent> {
        let key = format!("{tenant}:{entity_type}:{entity_id}");
        let log = self.entity_observe_log.lock().unwrap(); // ci-ok: infallible lock
        log.get(&key)
            .map(|entries| {
                entries
                    .iter()
                    .filter(|event| event.seq > since)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(crate) fn broadcast_agent_progress(&self, event: AgentProgressEvent) {
        let _ = self.agent_progress_tx.send(event.clone());
        let observe_event = EntityObserveEvent {
            tenant: event.tenant.clone(),
            entity_type: event.entity_type.clone(),
            entity_id: event.entity_id.clone(),
            seq: event.seq,
            event_name: event.kind.clone(),
            data: serde_json::to_value(&event).unwrap_or_default(),
        };
        self.push_entity_observe_event(observe_event);
    }

    /// Create ServerState with I/O Automaton TOML specs for transition table resolution.
    ///
    /// Returns an error if any IOA spec fails to parse.
    pub fn with_specs(
        system: ActorSystem,
        csdl: CsdlDocument,
        csdl_xml: String,
        ioa_sources: BTreeMap<String, String>,
    ) -> Result<Self, String> {
        let mut state = Self::new(system, csdl, csdl_xml);
        let mut tables = BTreeMap::new();
        for (entity_type, ioa_source) in &ioa_sources {
            let table = TransitionTable::try_from_ioa_source(ioa_source)
                .map_err(|e| format!("entity '{entity_type}': {e}"))?;
            tables.insert(entity_type.clone(), Arc::new(table));
        }
        state.transition_tables = Arc::new(tables);
        Ok(state)
    }

    /// Create ServerState with specs AND Postgres persistence.
    ///
    /// Returns an error if any IOA spec fails to parse.
    pub fn with_persistence(
        system: ActorSystem,
        csdl: CsdlDocument,
        csdl_xml: String,
        ioa_sources: BTreeMap<String, String>,
        store: PostgresEventStore,
    ) -> Result<Self, String> {
        let mut state = Self::with_specs(system, csdl, csdl_xml, ioa_sources)?;
        state.set_storage_stack(StorageStack::from_postgres(store));
        Ok(state)
    }

    /// Create ServerState with specs and an explicit storage stack.
    ///
    /// Returns an error if any IOA spec fails to parse.
    pub fn with_storage_stack(
        system: ActorSystem,
        csdl: CsdlDocument,
        csdl_xml: String,
        ioa_sources: BTreeMap<String, String>,
        stack: StorageStack,
    ) -> Result<Self, String> {
        let mut state = Self::with_specs(system, csdl, csdl_xml, ioa_sources)?;
        state.set_storage_stack(stack);
        Ok(state)
    }

    /// Create ServerState from a [`SpecRegistry`] in single-tenant compatibility mode.
    ///
    /// Used by tests and simple setups.  For multi-tenant production use
    /// [`from_registry_shared`](Self::from_registry_shared) instead.
    pub fn from_registry(system: ActorSystem, registry: SpecRegistry) -> Self {
        let mut state = Self::from_registry_shared(system, Arc::new(RwLock::new(registry)));
        state.single_tenant_mode = true;
        state
    }

    /// Create ServerState from a shared, mutable [`SpecRegistry`].
    ///
    /// Use this when the registry must be shared with another component
    /// (e.g. `PlatformState`) so that writes are visible to dispatch.
    pub fn from_registry_shared(system: ActorSystem, registry: Arc<RwLock<SpecRegistry>>) -> Self {
        let (event_tx, _) = tokio::sync::broadcast::channel(256); // determinism-ok: broadcast for external observation
        let (entity_observe_tx, _) = tokio::sync::broadcast::channel(512); // determinism-ok: broadcast for external observation
        let (design_time_tx, _) = tokio::sync::broadcast::channel(256); // determinism-ok: broadcast for external observation
        let (pending_decision_tx, _) = tokio::sync::broadcast::channel(256); // determinism-ok: broadcast for external observation
        let (agent_progress_tx, _) = tokio::sync::broadcast::channel(256); // determinism-ok: broadcast for external observation
        let (observe_refresh_tx, _) = tokio::sync::broadcast::channel(64); // determinism-ok: broadcast for external observation
        let state = Self {
            capture_health: crate::trajectory_outbox::CaptureHealth::default(),
            actor_system: Arc::new(system),
            pg_actor_system: None,
            actor_backed_types: BTreeSet::new(),
            csdl: Arc::new(CsdlDocument {
                version: "4.0".into(),
                schemas: vec![],
            }),
            csdl_xml: Arc::new(String::new()),
            entity_set_map: Arc::new(BTreeMap::new()),
            transition_tables: Arc::new(BTreeMap::new()),
            actor_registry: Arc::new(RwLock::new(BTreeMap::new())),
            last_accessed: Arc::new(RwLock::new(BTreeMap::new())),
            storage_stack: None,
            query_projection_queue: Arc::new(Mutex::new(None)),
            snapshot_write_queue: Arc::new(Mutex::new(None)),
            ots_trajectory_outbox: Arc::new(Mutex::new(None)),
            data_dir: std::path::PathBuf::new(),
            agent_hints: Arc::new(RwLock::new(BTreeMap::new())),
            // Missing tenant policy state is never an implicit permit-all
            // compatibility mode (ARN-230).
            authz: Arc::new(AuthzEngine::empty()),
            registry,
            entity_index: Arc::new(RwLock::new(BTreeMap::new())),
            entity_index_hydrated: Arc::new(RwLock::new(BTreeSet::new())),
            key_index_backfilled: Arc::new(RwLock::new(BTreeMap::new())),
            key_index_watermarks_loaded: Arc::new(RwLock::new(BTreeSet::new())),
            event_tx: Arc::new(event_tx),
            entity_observe_tx: Arc::new(entity_observe_tx),
            start_time: sim_now(),
            metrics: Arc::new(MetricsCollector::new()),
            record_store: Arc::new(RecordStore::new()),
            pg_record_store: None,
            reaction_dispatcher: Arc::new(RwLock::new(None)),
            reaction_recovery_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            reaction_recovery_owner: Arc::new(()),
            schema_migration_supervisor_tx: Arc::new(Mutex::new(None)),
            schema_migration_supervisor_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            schema_migration_supervisor_owner: Arc::new(()),
            webhook_dispatcher: None,
            adapter_registry: Arc::new(AdapterRegistry::with_builtins()),
            wasm_module_registry: Arc::new(RwLock::new(WasmModuleRegistry::new())),
            wasm_engine: Arc::new(WasmEngine::default()),
            cross_invariant_enforce: env_bool("TEMPER_XINV_ENFORCE", true),
            cross_invariant_eventual_enforce: env_bool("TEMPER_XINV_EVENTUAL_ENFORCE", true),
            design_time_tx: Arc::new(design_time_tx),
            entity_state_cache: Arc::new(Mutex::new(lru::LruCache::new(
                NonZeroUsize::new(state_cache_budget()).expect("cache budget must be > 0"),
            ))),
            action_dispatch_timeout: env_timeout(),
            admission: Arc::new(AdmissionController::new()),
            state_timeout_tracker: Arc::new(StateTimeoutTracker::new()),
            eventual_tracker: Arc::new(RwLock::new(
                crate::eventual_invariants::EventualInvariantTracker::new(),
            )),
            idempotency_cache: Arc::new(IdempotencyCache::new()),
            internal_invocation_credentials: InternalInvocationCredentialStore::runtime(),
            pending_decision_tx: Arc::new(pending_decision_tx),
            tenant_policies: Arc::new(RwLock::new(BTreeMap::new())),
            #[cfg(feature = "observe")]
            policy_approval_lock: Arc::new(tokio::sync::Mutex::new(())),
            commons_guardrail_tenants: Arc::new(RwLock::new(BTreeSet::new())),
            commons_rate_limit_buckets: Arc::new(Mutex::new(BTreeMap::new())),
            commons_storage_projection_cache: Arc::new(Mutex::new(BTreeMap::new())),
            commons_storage_reservations: Arc::new(Mutex::new(BTreeMap::new())),
            commons_write_guardrail_lock: Arc::new(tokio::sync::Mutex::new(())),
            raw_blob_ingest_budget: crate::blob_store::BlobIngestBudget::runtime(),
            secrets_vault: None,
            agent_progress_tx: Arc::new(agent_progress_tx), // determinism-ok: broadcast for external observation
            entity_event_sequences: Arc::new(Mutex::new(BTreeMap::new())),
            entity_observe_log: Arc::new(Mutex::new(BTreeMap::new())),
            observe_refresh_tx: Arc::new(observe_refresh_tx), // determinism-ok: broadcast for external observation
            listen_port: Arc::new(std::sync::OnceLock::new()),
            single_tenant_mode: false,
            suggestion_engine: Arc::new(RwLock::new(PolicySuggestionEngine::new())),
            verify_subprocess_bin: None,
            custom_effect_handler: None,
            bound_action_hook: None,
            http_endpoint_tables: Arc::new(crate::http_endpoint::HttpEndpointTables::new()),
            http_stream_registry: Arc::new(temper_wasm::http_stream::HttpStreamRegistry::new()),
            workflow_spans: Arc::new(crate::workflow_tracing::WorkflowSpanRegistry::default()),
            local_tdata_hosts: Arc::new(env_local_tdata_hosts()),
            llm_content_export_tenants: Arc::new(env_llm_content_export_tenants()),
        };
        state.register_builtin_wasm_modules();
        state
    }

    /// Attach a reaction dispatcher for cross-entity coordination.
    pub fn with_reaction_dispatcher(self, dispatcher: Arc<ReactionDispatcher>) -> Self {
        if let Ok(mut slot) = self.reaction_dispatcher.write() {
            *slot = Some(Arc::clone(&dispatcher));
        }
        self.spawn_reaction_recovery(dispatcher);
        self
    }

    /// Rebuild and install reaction dispatcher from the current spec registry.
    pub fn rebuild_reaction_dispatcher(&self) {
        let reaction_registry = {
            let registry = self.registry.read().unwrap();
            registry.build_reaction_registry()
        };
        let dispatcher = Arc::new(ReactionDispatcher::new(Arc::new(reaction_registry)));
        if let Ok(mut slot) = self.reaction_dispatcher.write() {
            *slot = Some(Arc::clone(&dispatcher));
        }
        self.spawn_reaction_recovery(dispatcher);
    }

    fn spawn_reaction_recovery(&self, dispatcher: Arc<ReactionDispatcher>) {
        if self.event_journal().is_none() || tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        // Recovery follows configured tenants, not current reaction rules:
        // committed intents carry their immutable rule and must remain
        // deliverable after the final live rule is removed.
        let tenants = self
            .registry
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .tenant_ids()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        if tenants.is_empty() {
            return;
        }
        let generation = self
            .reaction_recovery_generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            .saturating_add(1);
        let recovery_owner = Arc::downgrade(&self.reaction_recovery_owner);
        let mut state = self.clone();
        // The worker must not keep its own lifetime sentinel alive. All other
        // state is retained only until the next bounded owner check.
        state.reaction_recovery_owner = Arc::new(());
        // Bound-action hooks run only from the OData binding layer, never from
        // core reaction dispatch. Platform hooks may retain a PlatformState
        // (and therefore the original sentinel), so the worker clone must not
        // carry this unused ownership edge.
        state.bound_action_hook = None;
        tokio::spawn(async move {
            // determinism-ok: production startup recovery uses the durable
            // event-store contract; deterministic tests invoke the same scan
            // directly under their simulated scheduler.
            let now = tokio::time::Instant::now(); // determinism-ok: production supervisor cadence only
            let mut tenant_due = tenants
                .into_iter()
                .map(|tenant| (tenant, now))
                .collect::<BTreeMap<_, _>>();
            loop {
                if recovery_owner.upgrade().is_none()
                    || state
                        .reaction_recovery_generation
                        .load(std::sync::atomic::Ordering::SeqCst)
                        != generation
                {
                    return;
                }
                let now = tokio::time::Instant::now(); // determinism-ok: production supervisor cadence only
                for tenant in dispatcher.take_recovery_wake_tenants() {
                    tenant_due.insert(tenant, now);
                }
                let due_tenants = tenant_due
                    .iter()
                    .filter(|(_, due)| **due <= now)
                    .map(|(tenant, _)| tenant.clone())
                    .collect::<Vec<_>>();
                if due_tenants.is_empty() {
                    let next_due = tenant_due.values().min().copied().unwrap_or(now);
                    tokio::select! { // determinism-ok: production recovery cadence; eligibility uses sim_now
                        () = tokio::time::sleep_until(next_due) => {}
                        () = dispatcher.wait_for_recovery_signal() => {}
                    }
                    continue;
                }
                for tenant in due_tenants {
                    if recovery_owner.upgrade().is_none()
                        || state
                            .reaction_recovery_generation
                            .load(std::sync::atomic::Ordering::SeqCst)
                            != generation
                    {
                        return;
                    }
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(60),
                        dispatcher.recover_tenant_deliveries(&state, &tenant, 1_024),
                    )
                    .await
                    {
                        Ok(Ok(_)) => {
                            tenant_due.insert(
                                tenant.clone(),
                                tokio::time::Instant::now() // determinism-ok: production supervisor cadence only
                                    + dispatcher.recovery_supervisor_delay(&tenant),
                            );
                        }
                        Ok(Err(error)) => {
                            tracing::error!(tenant = %tenant, %error, "reaction recovery cycle failed");
                            tenant_due.insert(
                                tenant.clone(),
                                tokio::time::Instant::now() + std::time::Duration::from_secs(1), // determinism-ok: production error backoff only
                            );
                        }
                        Err(_) => {
                            tracing::warn!(tenant = %tenant, "reaction recovery cycle exhausted its wall-time budget");
                            tenant_due.insert(
                                tenant.clone(),
                                tokio::time::Instant::now() + std::time::Duration::from_secs(1), // determinism-ok: production timeout backoff only
                            );
                        }
                    }
                }
            }
        });
    }

    pub(crate) fn query_projection_fields(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        fields: &serde_json::Value,
    ) -> serde_json::Value {
        let Some(obj) = fields.as_object() else {
            return fields.clone();
        };
        let registry = self.registry.read().unwrap();
        let Some(spec) = registry.get_spec(tenant, entity_type) else {
            return fields.clone();
        };

        let mut projected = obj.clone();
        for state_var in &spec.automaton.state {
            if state_var.query_indexed == Some(false) {
                projected.remove(&state_var.name);
            }
        }

        serde_json::Value::Object(projected)
    }

    pub(crate) fn query_projection_state(
        &self,
        state: &crate::entity_actor::EntityState,
    ) -> serde_json::Value {
        let mut projected = match serde_json::to_value(state) {
            Ok(value) => value,
            Err(error) => {
                tracing::error!(
                    error = %error,
                    tenant = "unknown",
                    entity_type = %state.entity_type,
                    entity_id = %state.entity_id,
                    "failed to serialize query projection state"
                );
                return serde_json::json!({});
            }
        };
        if let Some(obj) = projected.as_object_mut() {
            obj.insert("events".to_string(), serde_json::json!([]));
        }
        projected
    }

    /// Attach a webhook dispatcher for external system notifications.
    pub fn with_webhook_dispatcher(mut self, dispatcher: Arc<WebhookDispatcher>) -> Self {
        self.webhook_dispatcher = Some(dispatcher);
        self
    }

    /// Override cross-invariant enforcement mode.
    pub fn with_cross_invariant_enforcement(
        mut self,
        enforce: bool,
        eventual_enforce: bool,
    ) -> Self {
        self.cross_invariant_enforce = enforce;
        self.cross_invariant_eventual_enforce = eventual_enforce;
        self
    }

    /// Attach a Postgres-backed evolution record store.
    pub fn with_pg_record_store(mut self, store: PostgresRecordStore) -> Self {
        self.pg_record_store = Some(Arc::new(store));
        self
    }

    /// Create ServerState from SpecRegistry using the PG-backed actor system.
    /// This is the actorized runtime path: actor_instances + actor_messages are source of truth.
    pub fn from_pg_registry(system: Arc<PgActorSystem>, registry: SpecRegistry) -> Self {
        let legacy = ActorSystem::new("pg-actor-compat");
        let mut state = Self::from_registry(legacy, registry);
        state.pg_actor_system = Some(system);
        state
    }

    /// Return true when OData for this tenant/entity should dispatch through
    /// the Postgres actor runtime.
    pub fn is_pg_actor_backed(&self, tenant: &TenantId, entity_type: &str) -> bool {
        let configured = self.actor_backed_types.contains(entity_type)
            || self
                .actor_backed_types
                .contains(&format!("{}:{entity_type}", tenant.as_str()));
        if !configured {
            return false;
        }
        // The generic PG actor runtime does not carry pre-resolved target
        // evidence. Contracted entities therefore use the canonical entity
        // actor, whose staged pre-commit validator covers every write origin.
        let registry = self
            .registry
            .read()
            .expect("spec registry lock should not be poisoned");
        let contracted = registry
            .get_spec(tenant, entity_type)
            .map(|spec| spec.table().clone())
            .or_else(|| self.transition_tables.get(entity_type).map(Arc::clone))
            .is_some_and(|table| {
                table
                    .state_var_metadata
                    .values()
                    .any(|metadata| metadata.var_type.as_deref() == Some("ref"))
                    || table.keys.iter().any(|key| key.entity_id)
            });
        !contracted
    }

    /// Attach an encrypted secrets vault.
    pub fn with_secrets_vault(mut self, vault: SecretsVault) -> Self {
        self.secrets_vault = Some(Arc::new(vault));
        self
    }

    /// Insert/update an entity status cache entry.
    ///
    /// The underlying [`lru::LruCache`] automatically evicts the least-recently-used
    /// entry when the budget (see [`state_cache_budget`]) is exceeded, so no manual
    /// eviction loop is needed here.
    pub fn cache_entity_status(&self, cache_key: String, status: String) {
        if let Ok(mut cache) = self.entity_state_cache.lock() {
            cache.put(cache_key, (status, sim_now()));
        }
    }

    /// Get a reference to the platform Turso event store.
    ///
    /// Panics if the event store is not configured or is not a Turso backend.
    pub fn turso(&self) -> temper_store_turso::TursoEventStore {
        self.platform_turso_store()
            .expect("Turso event store is not configured")
    }

    /// Get an optional reference to the **platform** Turso event store.
    ///
    /// Only use for system-wide data that stays in the platform DB
    /// (evolution records, feature requests, tenant registry).
    /// For tenant-scoped data, use [`turso_store_for_tenant`].
    ///
    /// Returns `None` when the server is running without Turso (e.g. in-memory
    /// mode or tests). Callers should degrade gracefully to empty results.
    pub fn platform_turso_store(&self) -> Option<temper_store_turso::TursoEventStore> {
        self.storage_stack
            .as_ref()
            .and_then(|stack| stack.turso.as_ref())
            .and_then(|provider| provider.platform_store())
    }

    /// Get a Turso store for a specific tenant.
    ///
    /// In TenantRouted mode, routes to the per-tenant database.
    /// In single-DB Turso mode, returns the shared store.
    /// `temper-system` and `default` tenants route to the platform store.
    ///
    /// Returns `None` when the server is running without Turso.
    pub async fn turso_store_for_tenant(
        &self,
        tenant: &str,
    ) -> Option<temper_store_turso::TursoEventStore> {
        let provider = self.storage_stack.as_ref()?.turso.as_ref()?.clone();
        provider.store_for_tenant(tenant).await
    }

    /// Return a backend-neutral metadata store for one tenant.
    ///
    /// Postgres is a shared platform store with tenant columns; Turso may be
    /// single-DB or tenant-routed. This helper lets read/write paths avoid
    /// branching on Turso-only accessors.
    pub async fn metadata_store_for_tenant(&self, tenant: &str) -> Option<Arc<dyn MetadataStore>> {
        if let Some(provider) = self
            .storage_stack
            .as_ref()
            .and_then(|stack| stack.metadata.clone())
        {
            return provider.store_for_tenant(tenant).await;
        }

        None
    }

    /// Return the platform metadata store for system-wide tables.
    pub fn platform_metadata_store(&self) -> Option<Arc<dyn MetadataStore>> {
        if let Some(provider) = self
            .storage_stack
            .as_ref()
            .and_then(|stack| stack.metadata.clone())
        {
            return provider.platform_store();
        }

        None
    }

    /// Download stream content for a TemperFS `File` entity without going
    /// back through loopback HTTP.
    ///
    /// This is the programmatic equivalent of `GET /tdata/Files('{id}')/$value`
    /// and keeps WASM-local fast paths aligned with the normal blob_adapter
    /// read contract.
    pub async fn get_file_stream_content(
        &self,
        tenant: &temper_runtime::tenant::TenantId,
        file_id: &str,
        agent_ctx: &crate::request_context::AgentContext,
    ) -> Result<(u16, Vec<u8>), String> {
        match self.read_file_stream_indexed(tenant, file_id).await? {
            IndexedFileStreamRead::Content { bytes, .. } => return Ok((200, bytes)),
            IndexedFileStreamRead::NoContent { .. } => return Ok((404, Vec::new())),
            IndexedFileStreamRead::MissingIndex => {
                tracing::warn!(
                    tenant = %tenant,
                    file_id,
                    "file stream projection missing; falling back to actor/WASM materialization"
                );
            }
            IndexedFileStreamRead::StaleIndex { content_hash, .. } => {
                tracing::warn!(
                    tenant = %tenant,
                    file_id,
                    content_hash = %content_hash,
                    "file stream projection blob missing; falling back to actor/WASM materialization"
                );
            }
        }

        let entity_state = serde_json::to_value(
            &self
                .get_tenant_entity_state(tenant, "File", file_id)
                .await
                .map_err(|e| format!("failed to load File('{file_id}') state: {e}"))?
                .state,
        )
        .map_err(|e| format!("failed to serialize File('{file_id}') state: {e}"))?;

        let has_content = entity_state
            .get("booleans")
            .and_then(|b| b.get("has_content"))
            .and_then(|v| v.as_bool())
            .or_else(|| {
                entity_state
                    .get("fields")
                    .and_then(|f| f.get("has_content"))
                    .and_then(|v| v.as_bool())
            })
            .unwrap_or(false);
        if !has_content {
            return Ok((404, Vec::new()));
        }

        let response_stream_id = format!("download-{}", temper_runtime::scheduler::sim_uuid());
        let streams = Arc::new(RwLock::new(StreamRegistry::default()));

        let inv_ctx = WasmInvocationContext {
            tenant: tenant.to_string(),
            entity_type: "File".to_string(),
            entity_id: file_id.to_string(),
            trigger_action: "StreamDownload".to_string(),
            wasm_module: Some("blob_adapter".to_string()),
            trigger_params: serde_json::json!({
                "stream_id": response_stream_id,
                "operation": "get",
            }),
            entity_state,
            agent_id: agent_ctx.agent_id.clone(),
            session_id: agent_ctx.session_id.clone(),
            integration_config: std::collections::BTreeMap::new(),
            trace_id: agent_ctx.trace_id.clone().unwrap_or_default(),
            workflow_root_entity_type: agent_ctx.workflow_root_entity_type.clone(),
            workflow_root_entity_id: agent_ctx.workflow_root_entity_id.clone(),
            workflow_run_id: agent_ctx.workflow_run_id.clone(),
            http_request: None,
        };

        let security_ctx = agent_ctx.security_ctx.as_ref().ok_or_else(|| {
            "blob_adapter requires the caller's authenticated security context".to_string()
        })?;
        let wasm_result = self
            .invoke_wasm_direct(
                tenant,
                "blob_adapter",
                inv_ctx,
                streams.clone(),
                security_ctx,
            )
            .await
            .map_err(|e| format!("blob_adapter download failed: {e}"))?;

        if !wasm_result.success {
            return Err(wasm_result
                .error
                .unwrap_or_else(|| "blob_adapter returned an unknown error".to_string()));
        }

        let bytes = streams
            .write()
            .map_err(|_| "stream registry lock was poisoned".to_string())?
            .take_stream(&response_stream_id)
            .unwrap_or_default();

        Ok((200, bytes))
    }

    /// Find an entity spec by name across all tenants.
    ///
    /// Returns the owning tenant and the IOA source string on success.
    /// Acquires a read lock on the spec registry.
    pub fn find_entity_ioa_source(
        &self,
        entity: &str,
    ) -> Option<(temper_runtime::tenant::TenantId, String)> {
        let registry = self.registry.read().unwrap(); // ci-ok: infallible lock
        for tenant_id in registry.tenant_ids() {
            if let Some(entity_spec) = registry.get_spec(tenant_id, entity) {
                return Some((tenant_id.clone(), entity_spec.ioa_source.clone()));
            }
        }
        None
    }

    /// Load aggregated unmet-intent failure groups for one tenant.
    pub async fn load_unmet_intent_rows_aggregated(
        &self,
        tenant: &str,
    ) -> (
        Vec<temper_store_turso::UnmetIntentAggRow>,
        std::collections::BTreeMap<String, String>,
    ) {
        let Some(store) = self.metadata_store_for_tenant(tenant).await else {
            return (Vec::new(), std::collections::BTreeMap::new());
        };

        let mut failures = Vec::new();
        let mut submitted_specs = std::collections::BTreeMap::new();

        match store.load_unmet_intent_rows(tenant).await {
            Ok(rows) => failures.extend(rows),
            Err(e) => {
                tracing::warn!(error = %e, backend = store.backend_name(), tenant, "failed to load unmet intent rows");
            }
        }
        match store.load_submit_spec_timestamps(tenant).await {
            Ok(map) => submitted_specs.extend(map),
            Err(e) => {
                tracing::warn!(error = %e, backend = store.backend_name(), tenant, "failed to load submit-spec timestamps");
            }
        }
        (failures, submitted_specs)
    }

    /// Count trajectory rows per tenant using fan-out across all metadata stores.
    ///
    /// Returns an empty map when Turso is not configured.
    pub async fn count_trajectories_by_tenant(&self) -> std::collections::BTreeMap<String, u64> {
        let stores = self.collect_all_metadata_stores().await;
        if stores.is_empty() {
            return std::collections::BTreeMap::new();
        }

        let mut counts = std::collections::BTreeMap::new();
        for store in &stores {
            match store.count_trajectories_by_tenant().await {
                Ok(c) => {
                    for (tenant, count) in c {
                        *counts.entry(tenant).or_insert(0) += count;
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, backend = store.backend_name(), "failed to count trajectories by tenant");
                }
            }
        }
        counts
    }

    /// Load trajectory entries owned by one tenant.
    pub async fn load_trajectory_entries(&self, tenant: &str, limit: i64) -> Vec<TrajectoryEntry> {
        let Some(store) = self.metadata_store_for_tenant(tenant).await else {
            return Vec::new();
        };

        let mut all_entries = Vec::new();
        match store.load_recent_trajectories(tenant, limit).await {
            Ok(rows) => {
                all_entries.extend(rows.into_iter().map(|r| TrajectoryEntry {
                    timestamp: r.created_at,
                    tenant: r.tenant,
                    entity_type: r.entity_type,
                    entity_id: r.entity_id,
                    action: r.action,
                    success: r.success,
                    from_status: r.from_status,
                    to_status: r.to_status,
                    error: r.error,
                    agent_id: r.agent_id,
                    session_id: r.session_id,
                    authz_denied: r.authz_denied,
                    denied_resource: r.denied_resource,
                    denied_module: r.denied_module,
                    source: r.source.as_deref().and_then(|s| match s {
                        "Entity" => Some(TrajectorySource::Entity),
                        "Platform" => Some(TrajectorySource::Platform),
                        "Authz" => Some(TrajectorySource::Authz),
                        _ => None,
                    }),
                    spec_governed: r.spec_governed,
                    agent_type: None,
                    request_body: r.request_body.and_then(|s| serde_json::from_str(&s).ok()),
                    intent: r.intent,
                    matched_policy_ids: r.matched_policy_ids,
                    capture_seq: r.capture_seq,
                }));
            }
            Err(e) => {
                tracing::warn!(error = %e, backend = store.backend_name(), tenant, "failed to load trajectories");
            }
        }
        // Sort by timestamp descending and limit
        all_entries.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        all_entries.truncate(limit as usize);
        all_entries
    }

    /// Collect all Turso stores for fan-out reads.
    ///
    /// In single-DB mode, returns just the shared store.
    /// In TenantRouted mode, returns the platform store + all connected tenant stores.
    /// Returns an empty vec when Turso is not configured.
    pub async fn all_turso_stores(&self) -> Vec<temper_store_turso::TursoEventStore> {
        let Some(provider) = self
            .storage_stack
            .as_ref()
            .and_then(|stack| stack.turso.clone())
        else {
            return Vec::new();
        };
        provider.all_stores().await
    }

    /// Collect backend-neutral platform/tenant stores for cross-tenant reads.
    pub async fn collect_all_metadata_stores(&self) -> Vec<Arc<dyn MetadataStore>> {
        let Some(provider) = self
            .storage_stack
            .as_ref()
            .and_then(|stack| stack.metadata.clone())
        else {
            return Vec::new();
        };
        provider.all_stores().await
    }
}

#[cfg(test)]
mod tests {
    use super::{
        normalize_local_tdata_host, parse_llm_content_export_tenants, tenant_exports_llm_content,
    };

    /// The redaction gate is only as good as the policy that opens it. An empty
    /// or unset opt-in list must export for nobody — a bug that flipped this to
    /// default-allow would leak every tenant's prompts while every redaction unit
    /// test stayed green.
    #[test]
    fn llm_content_export_is_deny_by_default() {
        let unset = parse_llm_content_export_tenants("");
        assert!(unset.is_empty());
        assert!(!tenant_exports_llm_content(&unset, "acme"));
        assert!(!tenant_exports_llm_content(&unset, ""));

        // Whitespace/empty entries must not be read as a tenant that opts in.
        let blank = parse_llm_content_export_tenants(" , ,\t,");
        assert!(blank.is_empty(), "got {blank:?}");
        assert!(!tenant_exports_llm_content(&blank, "acme"));
    }

    #[test]
    fn llm_content_export_opts_in_only_listed_tenants() {
        let set = parse_llm_content_export_tenants(" acme , globex ");
        assert!(
            tenant_exports_llm_content(&set, "acme"),
            "trimmed entry opts in"
        );
        assert!(tenant_exports_llm_content(&set, "globex"));
        assert!(
            !tenant_exports_llm_content(&set, "initech"),
            "an unlisted tenant must stay redacted"
        );
        // Not a prefix/substring match: neighbours of a listed tenant stay out.
        assert!(!tenant_exports_llm_content(&set, "acme2"));
        assert!(!tenant_exports_llm_content(&set, "acm"));
        assert!(
            !tenant_exports_llm_content(&set, "ACME"),
            "match is case-sensitive"
        );
    }

    #[test]
    fn llm_content_export_wildcard_opts_in_every_tenant() {
        let all = parse_llm_content_export_tenants("*");
        assert!(tenant_exports_llm_content(&all, "acme"));
        assert!(tenant_exports_llm_content(&all, "anything-else"));

        // The wildcard still applies when it arrives alongside named tenants.
        let mixed = parse_llm_content_export_tenants("acme,*");
        assert!(tenant_exports_llm_content(&mixed, "initech"));
    }

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
