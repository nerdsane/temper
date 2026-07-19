use temper_server::registry::SpecRegistry;
use temper_spec::csdl::parse_csdl;

pub(crate) const ITEM_IOA: &str = r#"
[automaton]
name = "Item"
states = ["New", "Ready", "Deleted"]
initial = "New"

[[state]]
name = "Slug"
type = "string"
initial = ""

[[state]]
name = "Embedding"
type = "string"
initial = ""

[[state]]
name = "EmbeddingModel"
type = "string"
initial = ""

[[key]]
name = "slug"
properties = ["Slug"]

[[vector]]
name = "embed"
property = "Embedding"
model_property = "EmbeddingModel"
dims = 4
metric = "cosine"

[[action]]
name = "Create"
kind = "input"
from = ["New"]
to = "Ready"
params = ["Slug", "Embedding", "EmbeddingModel"]

[[action]]
name = "Delete"
kind = "input"
from = ["Ready"]
to = "Deleted"

[[action]]
name = "Change"
kind = "input"
from = ["Ready"]
to = "Ready"
params = ["Slug", "Embedding", "EmbeddingModel"]
"#;

const CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Test" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Item">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Slug" Type="Edm.String"/>
        <Property Name="Embedding" Type="Edm.String"/>
        <Property Name="EmbeddingModel" Type="Edm.String"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="TestService">
        <EntitySet Name="Items" EntityType="Temper.Test.Item"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>
"#;

pub(crate) fn build_registry() -> SpecRegistry {
    build_registry_from_source(ITEM_IOA)
}

pub(crate) fn build_registry_from_source(source: &str) -> SpecRegistry {
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CSDL_XML).expect("CSDL parse"),
        CSDL_XML.to_string(),
        &[("Item", source)],
    );
    registry
}
