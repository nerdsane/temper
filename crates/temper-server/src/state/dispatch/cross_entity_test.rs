//! Tests for required cross-entity ref resolution (ARN-92 #2).

use crate::registry::SpecRegistry;
use crate::state::ServerState;
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::parse_csdl;

const CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.RequiredRefTest" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Doc">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
        <Property Name="landing_file_id" Type="Edm.String"/>
        <Property Name="child_ids" Type="Edm.String"/>
      </EntityType>
      <EntityType Name="File">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Docs" EntityType="Temper.RequiredRefTest.Doc"/>
        <EntitySet Name="Files" EntityType="Temper.RequiredRefTest.File"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

/// Doc with a *required* scalar cross-entity guard on `landing_file_id`.
const DOC_REQUIRED_SCALAR: &str = r#"
[automaton]
name = "Doc"
states = ["Draft", "Submitted"]
initial = "Draft"

[[state]]
name = "landing_file_id"
type = "string"
initial = ""

[[action]]
name = "Submit"
from = ["Draft"]
to = "Submitted"
guard = [
  { type = "cross_entity_state", entity_type = "File", entity_id_source = "landing_file_id", required_status = ["Ready"], required = true },
]
"#;

/// Doc with a *required* list cross-entity guard on `child_ids`.
const DOC_REQUIRED_LIST: &str = r#"
[automaton]
name = "Doc"
states = ["Draft", "Submitted"]
initial = "Draft"

[[state]]
name = "child_ids"
type = "string"
initial = "[]"

[[action]]
name = "Submit"
from = ["Draft"]
to = "Submitted"
guard = [
  { type = "cross_entity_state", entity_type = "File", entity_id_source = "child_ids", required_status = ["Ready"], required = true },
]
"#;

/// Doc with an *optional* list cross-entity guard on `child_ids` (legacy
/// vacuous-true blast radius preserved).
const DOC_OPTIONAL_LIST: &str = r#"
[automaton]
name = "Doc"
states = ["Draft", "Submitted"]
initial = "Draft"

[[state]]
name = "child_ids"
type = "string"
initial = "[]"

[[action]]
name = "Submit"
from = ["Draft"]
to = "Submitted"
guard = [
  { type = "cross_entity_state", entity_type = "File", entity_id_source = "child_ids", required_status = ["Ready"] },
]
"#;

async fn state_with(doc_ioa: &str, test_name: &str) -> (ServerState, TenantId) {
    let csdl = parse_csdl(CSDL).expect("CSDL parses");
    let mut registry = SpecRegistry::new();
    registry.register_tenant("default", csdl, CSDL.to_string(), &[("Doc", doc_ioa)]);
    let state = ServerState::from_registry(ActorSystem::new(test_name), registry);
    (state, TenantId::default())
}

#[tokio::test]
async fn required_empty_scalar_ref_fails_guard() {
    let (state, tenant) = state_with(DOC_REQUIRED_SCALAR, "required-empty-scalar").await;
    state
        .get_or_create_tenant_entity(
            &tenant,
            "Doc",
            "doc-1",
            serde_json::json!({ "landing_file_id": "" }),
        )
        .await
        .expect("create Doc");

    let resolved = state
        .resolve_cross_entity_guards(&tenant, "Doc", "doc-1", "Submit")
        .await;

    assert_eq!(
        resolved.get("__xref:File:landing_file_id"),
        Some(&false),
        "an empty required scalar ref must fail the guard, not pass vacuously"
    );
}

#[tokio::test]
async fn required_empty_list_ref_fails_guard() {
    let (state, tenant) = state_with(DOC_REQUIRED_LIST, "required-empty-list").await;
    state
        .get_or_create_tenant_entity(
            &tenant,
            "Doc",
            "doc-1",
            serde_json::json!({ "child_ids": [] }),
        )
        .await
        .expect("create Doc");

    let resolved = state
        .resolve_cross_entity_guards(&tenant, "Doc", "doc-1", "Submit")
        .await;

    assert_eq!(
        resolved.get("__xref:File:child_ids"),
        Some(&false),
        "an empty required list ref must fail the guard"
    );
}

#[tokio::test]
async fn optional_empty_list_ref_stays_vacuous_true() {
    let (state, tenant) = state_with(DOC_OPTIONAL_LIST, "optional-empty-list").await;
    state
        .get_or_create_tenant_entity(
            &tenant,
            "Doc",
            "doc-1",
            serde_json::json!({ "child_ids": [] }),
        )
        .await
        .expect("create Doc");

    let resolved = state
        .resolve_cross_entity_guards(&tenant, "Doc", "doc-1", "Submit")
        .await;

    assert_eq!(
        resolved.get("__xref:File:child_ids"),
        Some(&true),
        "an empty optional list ref must stay vacuous-true (preserve blast radius)"
    );
}
