use super::*;
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_spec::csdl::parse_csdl;

fn spec(name: &str, rule: Option<&str>) -> String {
    let mut source =
        format!("[automaton]\nname = '{name}'\nstates = ['Open', 'Closed']\ninitial = 'Open'\n");
    if let Some(rule) = rule {
        source.push_str(&format!(
            "[[invariant]]\nname = 'rule'\nassert = \"{rule}\"\n"
        ));
    }
    source
}

fn state(tenant: &str, specs: &[(&str, &str)]) -> PlatformState {
    let entities = specs.iter().map(|(name, _)| format!(
        "<EntityType Name='{name}'><Key><PropertyRef Name='Id'/></Key><Property Name='Id' Type='Edm.String' Nullable='false'/><Property Name='status' Type='Edm.String'/></EntityType>"
    )).collect::<String>();
    let xml = format!(
        "<edmx:Edmx Version='4.0' xmlns:edmx='http://docs.oasis-open.org/odata/ns/edmx'><edmx:DataServices><Schema Namespace='Test' xmlns='http://docs.oasis-open.org/odata/ns/edm'>{entities}</Schema></edmx:DataServices></edmx:Edmx>"
    );
    let mut registry = SpecRegistry::new();
    registry.register_tenant(TenantId::new(tenant), parse_csdl(&xml).unwrap(), xml, specs);
    PlatformState::with_registry(registry, None)
}

#[tokio::test]
async fn dependency_update_rechecks_installed_rules_before_writes() {
    let parent = spec("Parent", None);
    let child = spec("Child", Some("Parent[Id].status != 'Closed'"));
    let state = state("alice", &[("Parent", &parent), ("Child", &child)]);
    let updated = format!(
        "{parent}\n[[action]]\nname = 'Close'\nkind = 'input'\nfrom = ['Open']\nto = 'Closed'\n"
    );
    let err = prove_to_the_end(
        &state,
        "alice",
        "parent-app",
        &[("Parent".into(), updated)],
        true,
        // The dependency change must be reachable under the proposed Cedar.
        Some("permit(principal, action, resource);"),
    )
    .await
    .unwrap_err();
    assert!(err.contains("Child.rule BROKEN"), "{err}");
    let registry = state.registry.read().unwrap();
    let installed = registry
        .get_spec(&TenantId::new("alice"), "Parent")
        .unwrap();
    assert!(
        installed.automaton.actions.is_empty(),
        "refusal must leave the installed spec intact"
    );
}

#[tokio::test]
async fn installed_dependencies_are_closed_transitively_and_tenant_scoped() {
    let b = spec("B", Some("C[Id].status != 'Closed'"));
    let c = spec("C", None);
    let state = state("alice", &[("B", &b), ("C", &c)]);
    let a = spec("A", Some("B[Id].status != 'Closed'"));
    let unproved = prove_to_the_end(
        &state,
        "alice",
        "a-app",
        &[("A".into(), a.clone())],
        true,
        None,
    )
    .await
    .unwrap();
    assert!(unproved.is_empty(), "{unproved:?}");
    // Bob's tenant runs no B, so A's rule cannot be proved there: the install
    // goes ahead and names the rule it could not prove.
    let unproved = prove_to_the_end(&state, "bob", "a-app", &[("A".into(), a)], true, None)
        .await
        .unwrap();
    assert_eq!(unproved.len(), 1, "{unproved:?}");
    assert!(
        unproved[0].contains("B, which this app does not declare"),
        "{unproved:?}"
    );
}

#[tokio::test]
async fn a_rule_that_checks_nothing_as_written_is_a_warning() {
    let b = spec("B", None);
    let state = state("alice", &[("B", &b)]);
    // A never moves to Closed, so the rule's premise holds nowhere.
    let a = spec("A", Some("status == 'Closed' => B[Id].status == 'Open'"));
    let unproved = prove_to_the_end(&state, "alice", "a-app", &[("A".into(), a)], true, None)
        .await
        .unwrap();
    assert_eq!(unproved.len(), 1, "{unproved:?}");
    assert!(
        unproved[0].contains("A.rule CHECKS NOTHING AS WRITTEN"),
        "{unproved:?}"
    );
}

/// A start installs again the specs and Cedar the tenant already runs: they
/// were proven when they were installed, so nothing is walked. The same
/// specs under Cedar the tenant does not run yet are walked.
#[tokio::test]
async fn an_install_that_changes_neither_specs_nor_cedar_proves_nothing() {
    let parent = format!(
        "{}\n[[action]]\nname = 'Close'\nkind = 'input'\nfrom = ['Open']\nto = 'Closed'\n",
        spec("Parent", None)
    );
    let child = spec("Child", Some("Parent[Id].status != 'Closed'"));
    let state = state("alice", &[("Parent", &parent), ("Child", &child)]);
    let running = "permit(principal, action, resource);";
    state
        .server
        .authz
        .reload_tenant_policies("alice", running)
        .expect("load policies");
    let specs = [("Parent".to_string(), parent), ("Child".to_string(), child)];

    let unproved = prove_to_the_end(&state, "alice", "app", &specs, false, Some(running))
        .await
        .expect("nothing changed, so nothing is walked");
    assert!(unproved.is_empty(), "{unproved:?}");

    let changed = format!("{running}\n// one more line");
    let err = prove_to_the_end(&state, "alice", "app", &specs, false, Some(&changed))
        .await
        .unwrap_err();
    assert!(err.contains("Child.rule BROKEN"), "{err}");
}
