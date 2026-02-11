//! Observe API routes for developer tooling.
//!
//! These endpoints expose internal Temper state for the observability frontend.
//! They are only available when the `observe` feature is enabled.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::get;
use axum::Router;
use serde::{Deserialize, Serialize};

use crate::state::ServerState;

/// Summary of a loaded spec.
#[derive(Serialize, Deserialize)]
pub struct SpecSummary {
    /// Tenant that owns this spec.
    pub tenant: String,
    /// Entity type name.
    pub entity_type: String,
    /// Valid status states.
    pub states: Vec<String>,
    /// Action names.
    pub actions: Vec<String>,
    /// Initial state.
    pub initial_state: String,
}

/// Full spec detail.
#[derive(Serialize, Deserialize)]
pub struct SpecDetail {
    /// Entity type name.
    pub entity_type: String,
    /// Valid status states.
    pub states: Vec<String>,
    /// Initial state.
    pub initial_state: String,
    /// Action details.
    pub actions: Vec<ActionDetail>,
    /// Invariant details.
    pub invariants: Vec<InvariantDetail>,
    /// State variable declarations.
    pub state_variables: Vec<StateVarDetail>,
}

/// Detail of a single action.
#[derive(Serialize, Deserialize)]
pub struct ActionDetail {
    /// Action name.
    pub name: String,
    /// Action kind (input/output/internal).
    pub kind: String,
    /// States from which this action can fire.
    pub from: Vec<String>,
    /// Target state after firing.
    pub to: Option<String>,
    /// Guard conditions (Debug representation).
    pub guards: Vec<String>,
    /// Effects (Debug representation).
    pub effects: Vec<String>,
}

/// Detail of a single invariant.
#[derive(Serialize, Deserialize)]
pub struct InvariantDetail {
    /// Invariant name.
    pub name: String,
    /// Trigger states (empty = always checked).
    pub when: Vec<String>,
    /// Assertion expression.
    pub assertion: String,
}

/// Detail of a state variable.
#[derive(Serialize, Deserialize)]
pub struct StateVarDetail {
    /// Variable name.
    pub name: String,
    /// Variable type.
    pub var_type: String,
    /// Initial value.
    pub initial: String,
}

/// Entity instance summary.
#[derive(Serialize, Deserialize)]
pub struct EntityInstanceSummary {
    /// Entity type.
    pub entity_type: String,
    /// Entity ID.
    pub entity_id: String,
    /// Current status.
    pub status: String,
}

/// Build the observe router (mounted at /observe).
pub fn build_observe_router() -> Router<ServerState> {
    Router::new()
        .route("/specs", get(list_specs))
        .route("/specs/{entity}", get(get_spec_detail))
        .route("/entities", get(list_entities))
}

/// GET /observe/specs -- list all loaded specs across all tenants.
async fn list_specs(State(state): State<ServerState>) -> Json<Vec<SpecSummary>> {
    let registry = state.registry.read().unwrap();
    let mut specs = Vec::new();

    for tenant_id in registry.tenant_ids() {
        for entity_type in registry.entity_types(tenant_id) {
            if let Some(entity_spec) = registry.get_spec(tenant_id, entity_type) {
                let automaton = &entity_spec.automaton;
                specs.push(SpecSummary {
                    tenant: tenant_id.as_str().to_string(),
                    entity_type: entity_type.to_string(),
                    states: automaton.automaton.states.clone(),
                    actions: automaton.actions.iter().map(|a| a.name.clone()).collect(),
                    initial_state: automaton.automaton.initial.clone(),
                });
            }
        }
    }

    Json(specs)
}

/// GET /observe/specs/{entity} -- full spec detail for a named entity type.
///
/// Searches across all tenants and returns the first match.
async fn get_spec_detail(
    State(state): State<ServerState>,
    Path(entity): Path<String>,
) -> Result<Json<SpecDetail>, StatusCode> {
    let registry = state.registry.read().unwrap();

    for tenant_id in registry.tenant_ids() {
        if let Some(entity_spec) = registry.get_spec(tenant_id, &entity) {
            let automaton = &entity_spec.automaton;
            let detail = SpecDetail {
                entity_type: entity.clone(),
                states: automaton.automaton.states.clone(),
                initial_state: automaton.automaton.initial.clone(),
                actions: automaton
                    .actions
                    .iter()
                    .map(|a| ActionDetail {
                        name: a.name.clone(),
                        kind: a.kind.clone(),
                        from: a.from.clone(),
                        to: a.to.clone(),
                        guards: a.guard.iter().map(|g| format!("{g:?}")).collect(),
                        effects: a.effect.iter().map(|e| format!("{e:?}")).collect(),
                    })
                    .collect(),
                invariants: automaton
                    .invariants
                    .iter()
                    .map(|i| InvariantDetail {
                        name: i.name.clone(),
                        when: i.when.clone(),
                        assertion: i.assert.clone(),
                    })
                    .collect(),
                state_variables: automaton
                    .state
                    .iter()
                    .map(|sv| StateVarDetail {
                        name: sv.name.clone(),
                        var_type: sv.var_type.clone(),
                        initial: sv.initial.clone(),
                    })
                    .collect(),
            };
            return Ok(Json(detail));
        }
    }

    Err(StatusCode::NOT_FOUND)
}

/// GET /observe/entities -- list active entity instances from the actor registry.
async fn list_entities(State(state): State<ServerState>) -> Json<Vec<EntityInstanceSummary>> {
    let registry = state.actor_registry.read().unwrap();
    let entities: Vec<EntityInstanceSummary> = registry
        .keys()
        .map(|key| {
            // Actor keys are formatted as "{tenant}:{entity_type}:{entity_id}"
            let parts: Vec<&str> = key.splitn(3, ':').collect();
            EntityInstanceSummary {
                entity_type: parts.get(1).unwrap_or(&"unknown").to_string(),
                entity_id: parts.get(2).unwrap_or(&"unknown").to_string(),
                status: "active".to_string(),
            }
        })
        .collect();
    Json(entities)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use temper_runtime::ActorSystem;
    use temper_spec::csdl::parse_csdl;
    use tower::ServiceExt;

    use crate::registry::SpecRegistry;

    const CSDL_XML: &str = include_str!("../../../test-fixtures/specs/model.csdl.xml");
    const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");

    fn test_state_with_registry() -> ServerState {
        let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
        let mut registry = SpecRegistry::new();
        registry.register_tenant(
            "default",
            csdl,
            CSDL_XML.to_string(),
            &[("Order", ORDER_IOA)],
        );
        let system = ActorSystem::new("test-observe");
        ServerState::from_registry(system, registry)
    }

    fn build_test_app() -> Router {
        let state = test_state_with_registry();
        Router::new()
            .nest("/observe", build_observe_router())
            .with_state(state)
    }

    #[tokio::test]
    async fn test_list_specs_returns_registered_entities() {
        let app = build_test_app();
        let response = app
            .oneshot(Request::get("/observe/specs").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let specs: Vec<SpecSummary> = serde_json::from_slice(&body).unwrap();
        assert!(!specs.is_empty());
        assert_eq!(specs[0].entity_type, "Order");
        assert!(!specs[0].states.is_empty());
        assert!(!specs[0].actions.is_empty());
    }

    #[tokio::test]
    async fn test_get_spec_detail_found() {
        let app = build_test_app();
        let response = app
            .oneshot(
                Request::get("/observe/specs/Order")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let detail: SpecDetail = serde_json::from_slice(&body).unwrap();
        assert_eq!(detail.entity_type, "Order");
        assert!(!detail.states.is_empty());
        assert!(!detail.actions.is_empty());
    }

    #[tokio::test]
    async fn test_get_spec_detail_not_found() {
        let app = build_test_app();
        let response = app
            .oneshot(
                Request::get("/observe/specs/NonExistent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_list_entities_empty() {
        let app = build_test_app();
        let response = app
            .oneshot(
                Request::get("/observe/entities")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let entities: Vec<EntityInstanceSummary> = serde_json::from_slice(&body).unwrap();
        // No actors spawned yet, so should be empty
        assert!(entities.is_empty());
    }
}
