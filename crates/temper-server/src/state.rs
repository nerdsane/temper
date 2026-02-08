//! Server state shared across all request handlers.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use temper_jit::table::TransitionTable;
use temper_runtime::actor::ActorRef;
use temper_runtime::ActorSystem;
use temper_spec::csdl::CsdlDocument;
use temper_authz::{AuthzEngine, AuthzDecision, SecurityContext};
use temper_store_postgres::PostgresEventStore;

use crate::entity_actor::{EntityActor, EntityMsg, EntityResponse};

/// Shared state for the Temper HTTP server.
#[derive(Clone)]
pub struct ServerState {
    /// The actor system for spawning and managing entity actors.
    pub actor_system: Arc<ActorSystem>,
    /// Parsed CSDL document describing the entity model.
    pub csdl: Arc<CsdlDocument>,
    /// Raw CSDL XML string for serving via `$metadata`.
    pub csdl_xml: Arc<String>,
    /// Maps entity set names to entity type names.
    pub entity_set_map: Arc<HashMap<String, String>>,
    /// Transition table per entity type (built from TLA+ specs).
    pub transition_tables: Arc<HashMap<String, Arc<TransitionTable>>>,
    /// Live actor registry: (entity_type, entity_id) → ActorRef.
    pub actor_registry: Arc<RwLock<HashMap<String, ActorRef<EntityMsg>>>>,
    /// Optional Postgres event store for persistence.
    pub event_store: Option<Arc<PostgresEventStore>>,
    /// Agent hints learned from trajectory analysis, keyed by action name.
    pub agent_hints: Arc<RwLock<HashMap<String, String>>>,
    /// Cedar ABAC authorization engine.
    pub authz: Arc<AuthzEngine>,
}

impl ServerState {
    /// Create ServerState from CSDL XML and optional TLA+ sources.
    pub fn new(system: ActorSystem, csdl: CsdlDocument, csdl_xml: String) -> Self {
        let mut entity_set_map = HashMap::new();
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

        Self {
            actor_system: Arc::new(system),
            csdl: Arc::new(csdl),
            csdl_xml: Arc::new(csdl_xml),
            entity_set_map: Arc::new(entity_set_map),
            transition_tables: Arc::new(HashMap::new()),
            actor_registry: Arc::new(RwLock::new(HashMap::new())),
            event_store: None,
            agent_hints: Arc::new(RwLock::new(HashMap::new())),
            authz: Arc::new(AuthzEngine::permissive()),
        }
    }

    /// Create ServerState with TLA+ source for transition table resolution.
    pub fn with_tla(system: ActorSystem, csdl: CsdlDocument, csdl_xml: String, tla_sources: HashMap<String, String>) -> Self {
        let mut state = Self::new(system, csdl, csdl_xml);
        let mut tables = HashMap::new();
        for (entity_type, tla_source) in &tla_sources {
            let table = TransitionTable::from_tla_source(tla_source);
            tables.insert(entity_type.clone(), Arc::new(table));
        }
        state.transition_tables = Arc::new(tables);
        state
    }

    /// Create ServerState with TLA+ sources AND Postgres persistence.
    pub fn with_persistence(
        system: ActorSystem,
        csdl: CsdlDocument,
        csdl_xml: String,
        tla_sources: HashMap<String, String>,
        store: PostgresEventStore,
    ) -> Self {
        let mut state = Self::with_tla(system, csdl, csdl_xml, tla_sources);
        state.event_store = Some(Arc::new(store));
        state
    }

    /// Get or spawn an entity actor. Returns the ActorRef.
    pub fn get_or_spawn_actor(
        &self,
        entity_type: &str,
        entity_id: &str,
    ) -> Option<ActorRef<EntityMsg>> {
        let key = format!("{entity_type}:{entity_id}");

        // Check registry first
        {
            let registry = self.actor_registry.read().unwrap();
            if let Some(actor_ref) = registry.get(&key) {
                return Some(actor_ref.clone());
            }
        }

        // Get transition table for this entity type
        let table = self.transition_tables.get(entity_type)?;

        // Spawn new actor — with available infrastructure
        let actor = match &self.event_store {
            Some(pg) => EntityActor::with_persistence(
                entity_type, entity_id, table.clone(), serde_json::json!({}), pg.clone(),
            ),
            None => EntityActor::new(
                entity_type, entity_id, table.clone(), serde_json::json!({}),
            ),
        };
        let actor_ref = self.actor_system.spawn(actor, &key);

        // Register
        {
            let mut registry = self.actor_registry.write().unwrap();
            registry.insert(key, actor_ref.clone());
        }

        Some(actor_ref)
    }

    /// Check authorization for an action using the Cedar ABAC engine.
    pub fn authorize(&self, headers: &[(String, String)], action: &str, resource_type: &str, resource_attrs: &std::collections::HashMap<String, serde_json::Value>) -> Result<(), String> {
        let ctx = SecurityContext::from_headers(headers);
        let decision = self.authz.authorize_or_bypass(&ctx, action, resource_type, resource_attrs);
        match decision {
            AuthzDecision::Allow => Ok(()),
            AuthzDecision::Deny(reason) => Err(format!("Authorization denied: {reason}")),
        }
    }

    /// Dispatch an action to an entity actor and wait for the response.
    pub async fn dispatch_action(
        &self,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        params: serde_json::Value,
    ) -> Result<EntityResponse, String> {
        let actor_ref = self
            .get_or_spawn_actor(entity_type, entity_id)
            .ok_or_else(|| format!("No transition table for entity type '{entity_type}'"))?;

        actor_ref
            .ask::<EntityResponse>(
                EntityMsg::Action {
                    name: action.to_string(),
                    params,
                },
                Duration::from_secs(5),
            )
            .await
            .map_err(|e| format!("Actor dispatch failed: {e}"))
    }

    /// Get the current state of an entity actor.
    pub async fn get_entity_state(
        &self,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<EntityResponse, String> {
        let actor_ref = self
            .get_or_spawn_actor(entity_type, entity_id)
            .ok_or_else(|| format!("No transition table for entity type '{entity_type}'"))?;

        actor_ref
            .ask::<EntityResponse>(EntityMsg::GetState, Duration::from_secs(5))
            .await
            .map_err(|e| format!("Actor query failed: {e}"))
    }


    /// Update Agent.Hint annotations based on trajectory analysis.
    ///
    /// Stores hints in a map keyed by action name. Served via /odata/$hints.
    pub fn enrich_metadata(&self, action_name: &str, hint: &str) {
        self.agent_hints.write().unwrap().insert(action_name.to_string(), hint.to_string());
    }
}
