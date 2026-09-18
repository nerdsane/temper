use super::*;
use temper_spec::csdl::parse_csdl;

const CSDL_XML: &str = include_str!("../../../../test-fixtures/specs/model.csdl.xml");
const ORDER_IOA: &str = include_str!("../../../../test-fixtures/specs/order.ioa.toml");

fn minimal_csdl() -> (CsdlDocument, String) {
    let doc = parse_csdl(CSDL_XML).expect("CSDL should parse");
    (doc, CSDL_XML.to_string())
}

#[test]
fn register_and_lookup_tenant() {
    let mut registry = SpecRegistry::new();
    let (csdl, csdl_xml) = minimal_csdl();

    registry.register_tenant("alpha", csdl, csdl_xml, &[("Order", ORDER_IOA)]);

    let tenant = TenantId::new("alpha");
    assert!(registry.get_tenant(&tenant).is_some());
    assert!(registry.get_table(&tenant, "Order").is_some());
    assert!(registry.get_table(&tenant, "NonExistent").is_none());
}

#[test]
fn unknown_tenant_returns_none() {
    let registry = SpecRegistry::new();
    let tenant = TenantId::new("unknown");
    assert!(registry.get_tenant(&tenant).is_none());
    assert!(registry.get_table(&tenant, "Order").is_none());
}

#[test]
fn multiple_tenants_isolated() {
    let mut registry = SpecRegistry::new();
    let (csdl1, csdl_xml1) = minimal_csdl();
    let (csdl2, csdl_xml2) = minimal_csdl();

    registry.register_tenant("alpha", csdl1, csdl_xml1, &[("Order", ORDER_IOA)]);
    registry.register_tenant("beta", csdl2, csdl_xml2, &[("Task", ORDER_IOA)]);

    let a = TenantId::new("alpha");
    let b = TenantId::new("beta");

    // Each tenant sees only its own entities
    assert!(registry.get_table(&a, "Order").is_some());
    assert!(registry.get_table(&a, "Task").is_none());
    assert!(registry.get_table(&b, "Task").is_some());
    assert!(registry.get_table(&b, "Order").is_none());
}

#[test]
fn tenant_ids_listed() {
    let mut registry = SpecRegistry::new();
    let (csdl1, xml1) = minimal_csdl();
    let (csdl2, xml2) = minimal_csdl();

    registry.register_tenant("alpha", csdl1, xml1, &[]);
    registry.register_tenant("beta", csdl2, xml2, &[]);

    let ids: Vec<&str> = registry.tenant_ids().iter().map(|t| t.as_str()).collect();
    assert!(ids.contains(&"alpha"));
    assert!(ids.contains(&"beta"));
}

#[test]
fn entity_types_for_tenant() {
    let mut registry = SpecRegistry::new();
    let (csdl, xml) = minimal_csdl();

    registry.register_tenant("alpha", csdl, xml, &[("Order", ORDER_IOA)]);

    let types = registry.entity_types(&TenantId::new("alpha"));
    assert_eq!(types, vec!["Order"]);
}

#[test]
fn transition_table_is_functional() {
    let mut registry = SpecRegistry::new();
    let (csdl, xml) = minimal_csdl();

    registry.register_tenant("alpha", csdl, xml, &[("Order", ORDER_IOA)]);

    let table = registry
        .get_table(&TenantId::new("alpha"), "Order")
        .unwrap();
    assert_eq!(table.entity_name, "Order");
    assert_eq!(table.initial_state, "Draft");
    assert!(!table.rules.is_empty());

    // Verify it evaluates correctly
    let result = table.evaluate("Draft", 1, "SubmitOrder");
    assert!(result.is_some());
    assert!(result.unwrap().success);
}

#[test]
fn merge_register_identity_only_csdl_preserves_existing_relations() {
    // Regression (ARN-255): a merge=true registration of an identity-only
    // CSDL (no navigation relations) must NOT wipe the tenant's existing
    // relation graph. Before the fix, `relation_graph` was rebuilt from the
    // incoming csdl alone and assigned unconditionally, so FK validation,
    // restricted deletes, and $expand silently broke for the process
    // lifetime after any identity-spec merge (e.g. TrustedIssuer bootstrap).
    let mut registry = SpecRegistry::new();

    let relation_csdl = r#"<?xml version="1.0" encoding="UTF-8"?>
<edmx:Edmx Version="4.0"
  xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
<Schema Namespace="Temper.Rel"
  xmlns="http://docs.oasis-open.org/odata/ns/edm">
  <EntityType Name="Author">
    <Key><PropertyRef Name="Id"/></Key>
    <Property Name="Id" Type="Edm.Guid" Nullable="false"/>
  </EntityType>
  <EntityType Name="Book">
    <Key><PropertyRef Name="Id"/></Key>
    <Property Name="Id" Type="Edm.Guid" Nullable="false"/>
    <Property Name="AuthorId" Type="Edm.Guid" Nullable="false"/>
    <NavigationProperty Name="Author" Type="Temper.Rel.Author">
      <ReferentialConstraint Property="AuthorId" ReferencedProperty="Id"/>
    </NavigationProperty>
  </EntityType>
  <EntityContainer Name="RelService">
    <EntitySet Name="Authors" EntityType="Temper.Rel.Author"/>
    <EntitySet Name="Books" EntityType="Temper.Rel.Book"/>
  </EntityContainer>
</Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;
    let author_ioa =
        "[automaton]\nname = \"Author\"\nstates = [\"Created\"]\ninitial = \"Created\"\n";
    let book_ioa = "[automaton]\nname = \"Book\"\nstates = [\"Created\"]\ninitial = \"Created\"\n";

    registry.register_tenant(
        "rel-tenant",
        parse_csdl(relation_csdl).expect("relation CSDL should parse"),
        relation_csdl.to_string(),
        &[("Author", author_ioa), ("Book", book_ioa)],
    );

    let tenant = TenantId::new("rel-tenant");
    let edge_present = |registry: &SpecRegistry| {
        registry
            .get_tenant(&tenant)
            .and_then(|tc| tc.relation_graph.outgoing.get("Book"))
            .is_some_and(|edges| {
                edges
                    .iter()
                    .any(|e| e.navigation_property == "Author" && e.to_entity == "Author")
            })
    };
    assert!(
        edge_present(&registry),
        "Book->Author relation should exist after initial registration"
    );

    // Merge-register an identity-only CSDL with no navigation relations.
    let identity_csdl = r#"<?xml version="1.0" encoding="UTF-8"?>
<edmx:Edmx Version="4.0"
  xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
<Schema Namespace="Temper.Ident"
  xmlns="http://docs.oasis-open.org/odata/ns/edm">
  <EntityType Name="TrustedIssuer">
    <Key><PropertyRef Name="Id"/></Key>
    <Property Name="Id" Type="Edm.String" Nullable="false"/>
  </EntityType>
  <EntityContainer Name="IdentService">
    <EntitySet Name="TrustedIssuers" EntityType="Temper.Ident.TrustedIssuer"/>
  </EntityContainer>
</Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;
    let issuer_ioa =
        "[automaton]\nname = \"TrustedIssuer\"\nstates = [\"Active\"]\ninitial = \"Active\"\n";

    registry
        .try_register_tenant_with_reactions_and_constraints(
            "rel-tenant",
            parse_csdl(identity_csdl).expect("identity CSDL should parse"),
            identity_csdl.to_string(),
            &[("TrustedIssuer", issuer_ioa)],
            Vec::new(),
            None,
            true, // merge
        )
        .expect("merge registration should succeed");

    assert!(
        edge_present(&registry),
        "Book->Author relation MUST survive a merge of an identity-only CSDL"
    );
    assert!(
        registry.get_table(&tenant, "TrustedIssuer").is_some(),
        "merged identity entity should be registered"
    );
    assert!(
        registry.get_table(&tenant, "Book").is_some(),
        "existing Book entity should survive the merge"
    );
}

#[test]
fn remove_tenant_succeeds() {
    let mut registry = SpecRegistry::new();
    let (csdl, xml) = minimal_csdl();

    registry.register_tenant("doomed", csdl, xml, &[("Order", ORDER_IOA)]);
    let tenant = TenantId::new("doomed");
    assert!(registry.get_tenant(&tenant).is_some());

    assert!(registry.remove_tenant(&tenant));
    assert!(registry.get_tenant(&tenant).is_none());
    assert!(registry.get_table(&tenant, "Order").is_none());
}

#[test]
fn remove_nonexistent_tenant_returns_false() {
    let mut registry = SpecRegistry::new();
    let tenant = TenantId::new("nonexistent");
    assert!(!registry.remove_tenant(&tenant));
}

#[test]
fn spec_metadata_accessible() {
    let mut registry = SpecRegistry::new();
    let (csdl, xml) = minimal_csdl();

    registry.register_tenant("alpha", csdl, xml, &[("Order", ORDER_IOA)]);

    let spec = registry.get_spec(&TenantId::new("alpha"), "Order").unwrap();
    assert_eq!(spec.automaton.automaton.name, "Order");
    assert!(!spec.ioa_source.is_empty());
}

/// Minimal CSDL with a single EntityType + EntitySet for merge tests.
fn task_csdl() -> (CsdlDocument, String) {
    let xml = r#"<?xml version="1.0"?>
    <edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
      <edmx:DataServices>
        <Schema Namespace="Temper.Example" xmlns="http://docs.oasis-open.org/odata/ns/edm">
          <EntityType Name="Task">
            <Key><PropertyRef Name="Id"/></Key>
            <Property Name="Id" Type="Edm.Guid" Nullable="false"/>
          </EntityType>
          <EntityContainer Name="ExampleService">
            <EntitySet Name="Tasks" EntityType="Temper.Example.Task"/>
          </EntityContainer>
        </Schema>
      </edmx:DataServices>
    </edmx:Edmx>"#;
    (parse_csdl(xml).unwrap(), xml.to_string())
}

#[test]
fn merge_preserves_existing_entities_and_entity_set_map() {
    let mut registry = SpecRegistry::new();
    let (csdl, xml) = minimal_csdl();
    registry.register_tenant("alpha", csdl, xml, &[("Order", ORDER_IOA)]);
    let tenant = TenantId::new("alpha");

    let (new_csdl, new_xml) = task_csdl();
    registry
        .try_register_tenant_with_reactions_and_constraints(
            "alpha",
            new_csdl,
            new_xml,
            &[("Task", ORDER_IOA)],
            Vec::new(),
            None,
            true,
        )
        .expect("merge should succeed");

    assert!(
        registry.get_table(&tenant, "Order").is_some(),
        "Order survives merge"
    );
    assert!(
        registry.get_table(&tenant, "Task").is_some(),
        "Task added by merge"
    );

    let config = registry.get_tenant(&tenant).unwrap();
    assert!(config.entity_set_map.contains_key("Orders"));
    assert!(config.entity_set_map.contains_key("Tasks"));
    assert!(matches!(
        config.verification.get("Task"),
        Some(VerificationStatus::Pending)
    ));
}

#[test]
fn merge_with_no_cross_invariants_preserves_existing_ones() {
    // Regression: a follow-up merge that does not declare cross-invariants
    // (e.g. the Agent OS bootstrap running after a user app load) must not
    // wipe the ones already registered for the tenant. Observed live when
    // child entities on a Local parent returned 201 instead of 409 in the
    // Crucible walkthrough — the app load registered the rules, then the
    // agent-spec merge immediately erased them.
    const CROSS_INVARIANTS_TOML: &str = r#"
version = 1
default_delete_policy = "restrict"

[[invariant]]
name = "OrderStatusSanity"
kind = "hard"
on = "Order.*"
assert = 'related(Order, OrderId).status in ["Active"]'
"#;

    let mut registry = SpecRegistry::new();
    let (csdl, xml) = minimal_csdl();
    registry
        .try_register_tenant_with_reactions_and_constraints(
            "alpha",
            csdl,
            xml,
            &[("Order", ORDER_IOA)],
            Vec::new(),
            Some(CROSS_INVARIANTS_TOML.to_string()),
            false,
        )
        .expect("initial load should succeed");

    let tenant = TenantId::new("alpha");
    let initial_count = registry
        .get_tenant(&tenant)
        .unwrap()
        .cross_invariants
        .as_ref()
        .map(|c| c.invariants.len())
        .unwrap_or(0);
    assert_eq!(initial_count, 1, "sanity: cross-invariant registered");

    // Merge with cross_invariants_source = None (mimics agent OS bootstrap).
    let (new_csdl, new_xml) = task_csdl();
    registry
        .try_register_tenant_with_reactions_and_constraints(
            "alpha",
            new_csdl,
            new_xml,
            &[("Task", ORDER_IOA)],
            Vec::new(),
            None,
            true,
        )
        .expect("merge should succeed");

    let after_merge = registry
        .get_tenant(&tenant)
        .unwrap()
        .cross_invariants
        .as_ref()
        .map(|c| c.invariants.len())
        .unwrap_or(0);
    assert_eq!(
        after_merge, 1,
        "merge without cross-invariants must preserve existing ones"
    );
}

#[test]
fn replace_without_cross_invariants_clears_existing_ones() {
    // Replace mode is the opposite of merge: the caller is the full source
    // of truth, so a replace with `cross_invariants_source = None` must
    // clear any previously loaded rules.
    const CROSS_INVARIANTS_TOML: &str = r#"
version = 1
default_delete_policy = "restrict"

[[invariant]]
name = "OrderStatusSanity"
kind = "hard"
on = "Order.*"
assert = 'related(Order, OrderId).status in ["Active"]'
"#;

    let mut registry = SpecRegistry::new();
    let (csdl, xml) = minimal_csdl();
    registry
        .try_register_tenant_with_reactions_and_constraints(
            "alpha",
            csdl,
            xml,
            &[("Order", ORDER_IOA)],
            Vec::new(),
            Some(CROSS_INVARIANTS_TOML.to_string()),
            false,
        )
        .expect("initial load should succeed");

    let (csdl2, xml2) = minimal_csdl();
    registry
        .try_register_tenant_with_reactions_and_constraints(
            "alpha",
            csdl2,
            xml2,
            &[("Order", ORDER_IOA)],
            Vec::new(),
            None,
            false,
        )
        .expect("replace should succeed");

    let tenant = TenantId::new("alpha");
    assert!(
        registry
            .get_tenant(&tenant)
            .unwrap()
            .cross_invariants
            .is_none(),
        "replace mode must clear cross-invariants when the new payload has none"
    );
}

#[test]
fn replace_removes_entities_not_in_new_spec_set() {
    let mut registry = SpecRegistry::new();
    let (csdl, xml) = minimal_csdl();
    registry.register_tenant("alpha", csdl, xml, &[("Order", ORDER_IOA)]);
    let tenant = TenantId::new("alpha");

    let (csdl2, xml2) = minimal_csdl();
    registry
        .try_register_tenant_with_reactions_and_constraints(
            "alpha",
            csdl2,
            xml2,
            &[("Task", ORDER_IOA)],
            Vec::new(),
            None,
            false,
        )
        .expect("replace should succeed");

    assert!(
        registry.get_table(&tenant, "Order").is_none(),
        "Order removed in replace"
    );
    assert!(
        registry.get_table(&tenant, "Task").is_some(),
        "Task exists after replace"
    );
}
