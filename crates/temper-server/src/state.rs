//! Server state shared across all request handlers.

use std::collections::HashMap;
use std::sync::Arc;

use temper_runtime::ActorSystem;
use temper_spec::csdl::CsdlDocument;

/// Shared state for the Temper HTTP server.
/// Passed to all axum handlers via axum's State extractor.
#[derive(Clone)]
pub struct ServerState {
    /// The actor system for dispatching messages.
    pub actor_system: Arc<ActorSystem>,
    /// The CSDL document (for $metadata and entity set routing).
    pub csdl: Arc<CsdlDocument>,
    /// Raw CSDL XML for serving $metadata endpoint.
    pub csdl_xml: Arc<String>,
    /// Map of entity set name → entity type name.
    pub entity_set_map: Arc<HashMap<String, String>>,
}

impl ServerState {
    /// Create a new ServerState from CSDL XML source.
    pub fn new(system: ActorSystem, csdl: CsdlDocument, csdl_xml: String) -> Self {
        let mut entity_set_map = HashMap::new();

        // Build entity set → entity type mapping from CSDL
        for schema in &csdl.schemas {
            for container in &schema.entity_containers {
                for entity_set in &container.entity_sets {
                    // Entity type may be qualified ("Ns.Order") — extract short name
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
        }
    }
}
