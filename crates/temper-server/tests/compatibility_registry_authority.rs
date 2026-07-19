use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::json;
use temper_runtime::ActorSystem;
use temper_runtime::persistence::EventStore;
use temper_runtime::scheduler::install_deterministic_context;
use temper_runtime::tenant::TenantId;
use temper_server::StorageStack;
use temper_server::entity_actor::{EntityMsg, EntityResponse};
use temper_server::key_index::canonical_key_hash;
use temper_server::state::ServerState;
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

const CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Test" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="AuthorityItem">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="LegacyKey" Type="Edm.String"/>
        <Property Name="CurrentKey" Type="Edm.String"/>
        <Property Name="LegacyEmbedding" Type="Edm.String"/>
        <Property Name="LegacyModel" Type="Edm.String"/>
        <Property Name="CurrentEmbedding" Type="Edm.String"/>
        <Property Name="CurrentModel" Type="Edm.String"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="TestService">
        <EntitySet Name="AuthorityItems" EntityType="Temper.Test.AuthorityItem"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

const COMPATIBILITY_IOA: &str = r#"
[automaton]
name = "AuthorityItem"
states = ["Active"]
initial = "Active"

[[state]]
name = "LegacyKey"
type = "string"
initial = ""

[[state]]
name = "LegacyEmbedding"
type = "string"
initial = ""

[[state]]
name = "LegacyModel"
type = "string"
initial = ""

[[key]]
name = "legacy_key"
properties = ["LegacyKey"]

[[vector]]
name = "legacy_vector"
property = "LegacyEmbedding"
model_property = "LegacyModel"
dims = 2
metric = "cosine"

[[action]]
name = "Obsolete"
kind = "input"
from = ["Active"]
to = "Active"
"#;

const REGISTRY_IOA: &str = r#"
[automaton]
name = "AuthorityItem"
states = ["Active"]
initial = "Active"

[[state]]
name = "CurrentKey"
type = "string"
initial = ""

[[state]]
name = "CurrentEmbedding"
type = "string"
initial = ""

[[state]]
name = "CurrentModel"
type = "string"
initial = ""

[[key]]
name = "current_key"
properties = ["CurrentKey"]

[[vector]]
name = "current_vector"
property = "CurrentEmbedding"
model_property = "CurrentModel"
dims = 2
metric = "cosine"

[[action]]
name = "Current"
kind = "input"
from = ["Active"]
to = "Active"
"#;

fn action(name: &str) -> EntityMsg {
    EntityMsg::Action {
        name: name.to_string(),
        params: json!({}),
        cross_entity_booleans: BTreeMap::new(),
        idempotency_key: None,
        expected_spec_generation: None,
    }
}

#[tokio::test]
async fn cached_compatibility_actor_cannot_write_after_registry_authority_install() {
    let (_guard, _clock, _id_gen) = install_deterministic_context(189);
    let tenant = TenantId::default();
    let entity_id = "authority-item";
    let initial_fields = json!({
        "Id": entity_id,
        "LegacyKey": "legacy",
        "CurrentKey": "current",
        "LegacyEmbedding": [1.0, 0.0],
        "LegacyModel": "legacy-model",
        "CurrentEmbedding": [0.0, 1.0],
        "CurrentModel": "current-model"
    });
    let old_key_hash = canonical_key_hash(
        "legacy_key",
        &["LegacyKey".to_string()],
        initial_fields.as_object().expect("initial fields object"),
    )
    .expect("legacy key hash");
    let current_key_hash = canonical_key_hash(
        "current_key",
        &["CurrentKey".to_string()],
        initial_fields.as_object().expect("initial fields object"),
    )
    .expect("current key hash");

    let store = SimEventStore::no_faults(189);
    let state = ServerState::with_storage_stack(
        ActorSystem::new("compatibility-to-registry-authority"),
        parse_csdl(CSDL).expect("CSDL parse"),
        CSDL.to_string(),
        BTreeMap::from([("AuthorityItem".to_string(), COMPATIBILITY_IOA.to_string())]),
        StorageStack::from_sim(store.clone(), None),
    )
    .expect("compatibility state");

    let compatibility_actor = state
        .get_or_spawn_tenant_actor_with_fields(&tenant, "AuthorityItem", entity_id, initial_fields)
        .expect("compatibility actor");
    let _: EntityResponse = compatibility_actor
        .ask(EntityMsg::GetState, Duration::from_secs(1))
        .await
        .expect("start compatibility actor");

    let generation_guard = state.acquire_spec_generation_lock(&tenant).await;
    state
        .registry
        .write()
        .expect("registry lock")
        .register_tenant(
            tenant.clone(),
            parse_csdl(CSDL).expect("CSDL parse"),
            CSDL.to_string(),
            &[("AuthorityItem", REGISTRY_IOA)],
        );
    assert!(
        state.reconcile_declared_projections(&tenant).await,
        "new registry generation must reconcile before publication"
    );
    drop(generation_guard);

    let obsolete_result: Result<EntityResponse, _> = compatibility_actor
        .ask(action("Obsolete"), Duration::from_secs(1))
        .await;
    let obsolete_rejected = match obsolete_result {
        Ok(response) => !response.success,
        Err(_) => true,
    };
    let registry_actor = state
        .get_or_spawn_tenant_actor(&tenant, "AuthorityItem", entity_id)
        .expect("registry actor after registry install");
    let current: EntityResponse = registry_actor
        .ask(action("Current"), Duration::from_secs(1))
        .await
        .expect("current action response");

    let legacy_key_owner = store
        .lookup_by_key(
            tenant.as_str(),
            "AuthorityItem",
            "legacy_key",
            &old_key_hash,
        )
        .await
        .expect("legacy key lookup");
    let current_key_owner = store
        .lookup_by_key(
            tenant.as_str(),
            "AuthorityItem",
            "current_key",
            &current_key_hash,
        )
        .await
        .expect("current key lookup");
    let legacy_vectors = store
        .vector_candidates(
            tenant.as_str(),
            "AuthorityItem",
            "legacy_vector",
            "legacy-model",
            10,
        )
        .await
        .expect("legacy vector lookup");
    let current_vectors = store
        .vector_candidates(
            tenant.as_str(),
            "AuthorityItem",
            "current_vector",
            "current-model",
            10,
        )
        .await
        .expect("current vector lookup");
    let journal_actions = store
        .dump_journal(&format!("{tenant}:AuthorityItem:{entity_id}"))
        .into_iter()
        .map(|event| event.event_type)
        .collect::<Vec<_>>();

    assert!(
        obsolete_rejected
            && current.success
            && legacy_key_owner.is_none()
            && current_key_owner.as_deref() == Some(entity_id)
            && legacy_vectors.is_empty()
            && current_vectors.len() == 1
            && !journal_actions.iter().any(|action| action == "Obsolete")
            && journal_actions.iter().any(|action| action == "Current"),
        "cached compatibility actor remained authoritative after registry install: \
         obsolete_rejected={}, current_success={}, legacy_key_owner={:?}, \
         current_key_owner={:?}, legacy_vectors={}, current_vectors={}, journal={:?}",
        obsolete_rejected,
        current.success,
        legacy_key_owner,
        current_key_owner,
        legacy_vectors.len(),
        current_vectors.len(),
        journal_actions,
    );

    registry_actor.stop().expect("stop promoted actor");
    state.remove_entity(&tenant, "AuthorityItem", entity_id);
    let replayed_actor = state
        .get_or_spawn_tenant_actor(&tenant, "AuthorityItem", entity_id)
        .expect("registry actor for replay");
    let replayed: EntityResponse = replayed_actor
        .ask(EntityMsg::GetState, Duration::from_secs(1))
        .await
        .expect("replayed registry actor state");
    let replayed_obsolete: Result<EntityResponse, _> = replayed_actor
        .ask(action("Obsolete"), Duration::from_secs(1))
        .await;
    let replayed_obsolete_rejected = match replayed_obsolete {
        Ok(response) => !response.success,
        Err(_) => true,
    };
    let replayed_journal_actions = store
        .dump_journal(&format!("{tenant}:AuthorityItem:{entity_id}"))
        .into_iter()
        .map(|event| event.event_type)
        .collect::<Vec<_>>();

    assert!(
        replayed.success
            && replayed.state.sequence_nr == 2
            && replayed_obsolete_rejected
            && !replayed_journal_actions
                .iter()
                .any(|action| action == "Obsolete"),
        "registry authority was not replay-stable: replay_success={}, sequence_nr={}, \
         obsolete_rejected={}, journal={:?}",
        replayed.success,
        replayed.state.sequence_nr,
        replayed_obsolete_rejected,
        replayed_journal_actions,
    );
}
