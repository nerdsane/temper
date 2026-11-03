use temper_platform::os_apps::get_os_app;
use temper_platform::state::PlatformState;
use temper_runtime::tenant::TenantId;
use temper_server::identity::{IdentityResolver, hash_token};
use temper_server::storage::StorageStack;
use temper_spec::csdl::parse_csdl;
use temper_store_turso::TursoEventStore;

use super::{EarlierOperatorKeys, bootstrap_installed_apps, bootstrap_tenants};

#[tokio::test]
async fn agent_bootstrap_keeps_specs_restored_into_default() {
    const NOTE_CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Note">
        <Key><PropertyRef Name="id"/></Key>
        <Property Name="id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityContainer Name="Service">
        <EntitySet Name="Notes" EntityType="Temper.Note"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;
    const NOTE_IOA: &str =
        "[automaton]\nname = \"Note\"\nstates = [\"Draft\"]\ninitial = \"Draft\"\n";

    // Phase 2 restored a user spec into `default`.
    let state = PlatformState::new(None);
    state.registry.write().unwrap().register_tenant(
        "default",
        parse_csdl(NOTE_CSDL).expect("csdl"),
        NOTE_CSDL.to_string(),
        &[("Note", NOTE_IOA)],
    );

    bootstrap_tenants(&state, &[], EarlierOperatorKeys::Keep).await;

    let registry = state.registry.read().unwrap();
    let config = registry
        .get_tenant(&TenantId::new("default"))
        .expect("tenant default");
    assert_eq!(
        config.entity_set_map.get("Notes").map(String::as_str),
        Some("Note"),
        "agent bootstrap must not hide restored entity sets in default"
    );
    assert!(config.entity_set_map.contains_key("AgentTypes"));
}

/// ADR-0069: every tenant gets the platform's HttpEndpoint type at boot,
/// not only temper-system, so a server built from scratch can hold routes.
#[tokio::test]
async fn every_booted_tenant_has_the_http_endpoint_type() {
    let state = PlatformState::new(None);
    bootstrap_tenants(
        &state,
        &[("app-tenant".to_string(), String::new())],
        EarlierOperatorKeys::Keep,
    )
    .await;

    let registry = state.registry.read().unwrap();
    for tenant in ["default", "app-tenant"] {
        let config = registry
            .get_tenant(&TenantId::new(tenant))
            .unwrap_or_else(|| panic!("tenant {tenant}"));
        assert_eq!(
            config
                .entity_set_map
                .get("HttpEndpoints")
                .map(String::as_str),
            Some("HttpEndpoint"),
            "tenant {tenant} has no HttpEndpoints set"
        );
        assert!(
            registry
                .get_table(&TenantId::new(tenant), "HttpEndpoint")
                .is_some(),
            "tenant {tenant} has no HttpEndpoint table"
        );
    }
}

/// A boot with a new operator key, set to retire earlier keys
/// (`TEMPER_RETIRE_OLD_OPERATOR_KEYS=1`), revokes the credential the
/// previous boot issued for its key, and the new key keeps working.
#[tokio::test]
async fn a_boot_that_retires_earlier_operator_keys_revokes_the_previous_key() {
    const OLD_KEY: &str = "tmpr_operator-key-of-the-previous-boot";
    const NEW_KEY: &str = "tmpr_operator-key-of-this-boot";
    let tenant = TenantId::new("default");
    let mut state = PlatformState::new(None);
    state.api_token = Some(OLD_KEY.to_string());
    bootstrap_tenants(&state, &[], EarlierOperatorKeys::Keep).await;

    state.api_token = Some(NEW_KEY.to_string());
    bootstrap_tenants(&state, &[], EarlierOperatorKeys::Retire).await;

    let resolver = IdentityResolver::new();
    assert!(
        resolver
            .resolve(&state.server, &tenant, NEW_KEY)
            .await
            .is_ok(),
        "the current key is the operator"
    );
    assert!(
        resolver
            .resolve(&state.server, &tenant, OLD_KEY)
            .await
            .is_err(),
        "the previous boot's key no longer works"
    );
    let old = state
        .server
        .get_tenant_entity_state(&tenant, "AgentCredential", &hash_token(OLD_KEY))
        .await
        .expect("the previous boot's credential");
    assert_eq!(old.state.status, "Revoked");
}

#[tokio::test]
async fn bootstrap_installed_apps_replays_persisted_app_when_registry_specs_are_stale() {
    let tenant = "bootstrap-drift";
    let app_name = "temper-fs";
    let bundle = get_os_app(app_name).expect("temper-fs app bundle should load");
    let csdl_xml = bundle.csdl.clone().expect("temper-fs should have CSDL");
    let csdl = parse_csdl(&csdl_xml).expect("temper-fs CSDL should parse");
    let mut stale_specs: Vec<(String, String)> = bundle.specs.clone();
    stale_specs[0].1.push_str("\n# stale bootstrap copy\n");
    let stale_refs: Vec<(&str, &str)> = stale_specs
        .iter()
        .map(|(entity_type, source)| (entity_type.as_str(), source.as_str()))
        .collect();

    let mut state = PlatformState::new(None);
    {
        let mut registry = state.registry.write().unwrap();
        registry.register_tenant(tenant, csdl, csdl_xml, &stale_refs);
    }

    let db_path = std::env::temp_dir().join(format!(
        "temper-bootstrap-{}.db",
        temper_runtime::scheduler::sim_uuid()
    ));
    let db_url = format!("file:{}", db_path.display());
    let turso = TursoEventStore::new(&db_url, None)
        .await
        .expect("turso store should initialize");
    turso
        .record_installed_app(tenant, app_name)
        .await
        .expect("installed app should persist");
    state
        .server
        .set_storage_stack(StorageStack::from_turso(turso));

    bootstrap_installed_apps(&state, &[])
        .await
        .expect("bootstrap should replay persisted apps");

    let tenant_id = TenantId::new(tenant);
    let registry = state.registry.read().unwrap();
    let refreshed = registry
        .get_spec(&tenant_id, &bundle.specs[0].0)
        .expect("replayed app spec should remain registered");
    assert_eq!(
        refreshed.ioa_source, bundle.specs[0].1,
        "startup replay should refresh stale installed app specs from the current bundle"
    );

    let _ = std::fs::remove_file(&db_path);
}
