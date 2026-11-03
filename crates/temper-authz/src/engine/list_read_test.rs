//! A list read decided in two parts gives every row the decision the whole
//! policy set gives it.

use std::collections::HashMap;

use serde_json::json;

use super::*;
use crate::context::{Principal, PrincipalKind, SecurityContext};

fn reader(kind: PrincipalKind, role: Option<&str>) -> SecurityContext {
    SecurityContext {
        principal: Principal {
            id: "reader".to_string(),
            kind,
            role: role.map(str::to_string),
            acting_for: None,
            agent_type: None,
            label: None,
            attributes: HashMap::new(),
        },
        context_attrs: HashMap::new(),
        correlation_id: "list-read".to_string(),
        cause: None,
        credential: None,
    }
}

fn row(id: &str, owner: Option<&str>, status: &str) -> HashMap<String, serde_json::Value> {
    let mut attrs = HashMap::from([
        ("id".to_string(), json!(id)),
        ("status".to_string(), json!(status)),
    ]);
    if let Some(owner) = owner {
        attrs.insert("owner".to_string(), json!(owner));
    }
    attrs
}

fn rows() -> Vec<HashMap<String, serde_json::Value>> {
    let mut rows = Vec::new();
    for (index, owner) in [Some("reader"), Some("other"), None]
        .into_iter()
        .enumerate()
    {
        for status in ["Open", "Closed"] {
            rows.push(row(&format!("o{index}{status}"), owner, status));
        }
    }
    rows.push(row("o1", Some("other"), "Open"));
    rows
}

/// Each row's decision through the list read, beside the whole set's.
fn decisions(policy: &str, who: &SecurityContext) -> (Vec<bool>, Vec<bool>, &'static str) {
    let engine = AuthzEngine::empty();
    engine
        .reload_tenant_policies("tenant", policy)
        .expect("policy should parse");
    let probe = row("_list", None, "");
    let list = engine.list_read_for_tenant("tenant", who, "read", "Order", &probe);
    let mode = match &list {
        ListRead::AllowAll => "allow_all",
        ListRead::DenyAll => "deny_all",
        ListRead::PerRow(_) => "per_row",
    };
    let mut split = Vec::new();
    let mut whole = Vec::new();
    for attrs in rows() {
        split.push(match &list {
            ListRead::AllowAll => true,
            ListRead::DenyAll => false,
            ListRead::PerRow(rule) => {
                engine.list_row_allowed("tenant", who, "read", "Order", rule, &attrs)
            }
        });
        whole.push(
            engine
                .authorize_row_for_tenant("tenant", who, "read", "Order", &attrs)
                .is_allowed(),
        );
    }
    (split, whole, mode)
}

const POLICIES: &[&str] = &[
    // A list-wide permit and a forbid on a field each row fills.
    r#"permit(principal, action == Action::"read", resource is Order);
       forbid(principal, action == Action::"read", resource is Order)
       when { context has owner && context.owner != principal.id };"#,
    // Only a permit that reads the row.
    r#"permit(principal, action == Action::"read", resource is Order)
       when { resource has owner && resource.owner == principal.id };"#,
    // A list-wide forbid for one role beside a list-wide permit.
    r#"permit(principal, action == Action::"read", resource is Order);
       forbid(principal, action, resource is Order)
       when { principal has role && principal.role like "guest*" };"#,
    // A broad forbid that names the row's id, beside a list-wide permit.
    r#"permit(principal, action in [Action::"read", Action::"list"], resource is Order);
       forbid(principal, action, resource)
       when { principal has role && principal.role == "file-upload" }
       unless { resource has id && resource.id == principal.id };"#,
    // A permit for one row by its id, and one by status.
    r#"permit(principal, action == Action::"read", resource == Order::"o1");
       permit(principal, action == Action::"read", resource is Order)
       when { resource.status == "Open" };"#,
    // No permit at all.
    r#"forbid(principal, action == Action::"read", resource is Order)
       when { resource.status == "Closed" };"#,
    // A row read in a shape the classifier cannot project, beside a forbid on status.
    r#"permit(principal, action == Action::"read", resource is Order)
       when { resource has owner && resource.owner like "re*" };
       permit(principal is Agent, action, resource);
       forbid(principal, action == Action::"read", resource is Order)
       when { context has status && context.status == "Closed" };"#,
    // Policies for other types and actions only.
    r#"permit(principal, action == Action::"read", resource is Invoice);
       permit(principal, action == Action::"Pay", resource is Order);"#,
];

#[test]
fn every_row_gets_the_whole_sets_decision() {
    let readers = [
        reader(PrincipalKind::Customer, None),
        reader(PrincipalKind::Customer, Some("guest:link")),
        reader(PrincipalKind::Agent, Some("file-upload")),
        reader(PrincipalKind::Agent, None),
    ];
    for policy in POLICIES {
        for who in &readers {
            let (split, whole, mode) = decisions(policy, who);
            assert_eq!(
                split, whole,
                "{mode} differs for {:?} under:\n{policy}",
                who.principal
            );
        }
    }
}

/// The cases above cover all three outcomes, so a split that always fell
/// back to one of them would not pass for the right reason.
#[test]
fn the_cases_reach_every_outcome() {
    let customer = reader(PrincipalKind::Customer, None);
    let guest = reader(PrincipalKind::Customer, Some("guest:link"));
    assert_eq!(decisions(POLICIES[0], &customer).2, "per_row");
    assert_eq!(decisions(POLICIES[2], &customer).2, "allow_all");
    assert_eq!(decisions(POLICIES[2], &guest).2, "deny_all");
    assert_eq!(decisions(POLICIES[5], &customer).2, "deny_all");
}

/// A row is checked only against the policies that can tell rows apart.
#[test]
fn a_row_evaluates_only_the_policies_it_can_change() {
    let engine = AuthzEngine::empty();
    let mut policy = String::new();
    for index in 0..50 {
        policy.push_str(&format!(
            "permit(principal, action == Action::\"read\", resource is Order) when {{ principal.id == \"reader{index}\" || principal.id == \"reader\" }};\n"
        ));
    }
    policy.push_str(
        r#"forbid(principal, action == Action::"read", resource is Order)
           when { resource has owner && resource.owner != principal.id };"#,
    );
    engine
        .reload_tenant_policies("tenant", &policy)
        .expect("policy should parse");
    let who = reader(PrincipalKind::Customer, None);
    let probe = row("_list", None, "");
    let ListRead::PerRow(rule) =
        engine.list_read_for_tenant("tenant", &who, "read", "Order", &probe)
    else {
        panic!("a forbid on the owner tells rows apart");
    };
    // The 50 permits read only the principal, so they are decided once.
    assert_eq!(rule.row_policy_count, 1);
    assert!(engine.list_row_allowed(
        "tenant",
        &who,
        "read",
        "Order",
        &rule,
        &row("a", Some("reader"), "Open")
    ));
    assert!(!engine.list_row_allowed(
        "tenant",
        &who,
        "read",
        "Order",
        &rule,
        &row("b", Some("other"), "Open")
    ));
}

/// Policies a list read can meet: permits and forbids that read the row and
/// that do not, ones that error on rows missing a field, an id scope, a
/// forbid naming `resource.id`, and a nested read the classifier cannot
/// project.
const POOL: &[&str] = &[
    r#"permit(principal, action == Action::"read", resource is Order);"#,
    r#"permit(principal, action == Action::"read", resource is Order) when { resource.owner == principal.id };"#,
    r#"permit(principal, action == Action::"read", resource is Order) when { resource.status == "Open" };"#,
    r#"permit(principal, action, resource) when { principal has role && principal.role == "helper" };"#,
    r#"permit(principal, action == Action::"read", resource == Order::"o1");"#,
    r#"permit(principal, action == Action::"read", resource is Order) when { resource has owner && resource.owner.name == "x" };"#,
    r#"forbid(principal, action == Action::"read", resource is Order) when { context has owner && context.owner != principal.id };"#,
    r#"forbid(principal, action, resource is Order) when { principal has role && principal.role like "guest*" };"#,
    r#"forbid(principal, action, resource) when { principal has role && principal.role == "file-upload" } unless { resource has id && resource.id == principal.id };"#,
    r#"forbid(principal, action == Action::"read", resource is Order) when { resource.status == "Closed" };"#,
    r#"forbid(principal, action == Action::"read", resource is Order) when { resource.owner like "ot*" };"#,
    r#"forbid(principal, action, resource) unless { principal has role || principal is Customer };"#,
];

fn reader_as(id: &str, kind: PrincipalKind, role: Option<&str>) -> SecurityContext {
    let mut who = reader(kind, role);
    who.principal.id = id.to_string();
    who
}

/// Every set of one to three policies from [`POOL`], for principals with and
/// without roles, over rows with and without each field: the split's
/// decision for every row equals the whole set's. A split that dropped a
/// row-reading forbid, or let a list-wide permit pass a row's own forbid,
/// gives some row here a different answer.
#[test]
fn split_and_whole_set_agree_on_every_row() {
    let readers = [
        reader_as("reader", PrincipalKind::Customer, None),
        reader_as("reader", PrincipalKind::Customer, Some("guest:link")),
        reader_as("o1", PrincipalKind::Agent, Some("file-upload")),
        reader_as("reader", PrincipalKind::Agent, Some("helper")),
        reader_as("reader", PrincipalKind::Agent, None),
    ];
    let mut sets = Vec::new();
    for a in 0..POOL.len() {
        sets.push(vec![a]);
        for b in a + 1..POOL.len() {
            sets.push(vec![a, b]);
            for c in b + 1..POOL.len() {
                sets.push(vec![a, b, c]);
            }
        }
    }
    let mut compared = 0usize;
    let mut per_row = 0usize;
    for set in &sets {
        let policy = set
            .iter()
            .map(|index| POOL[*index])
            .collect::<Vec<_>>()
            .join("\n");
        for who in &readers {
            let (split, whole, mode) = decisions(&policy, who);
            assert_eq!(
                split, whole,
                "{mode} differs for {:?} under:\n{policy}",
                who.principal
            );
            compared += split.len();
            per_row += usize::from(mode == "per_row");
        }
    }
    // 298 sets x 5 principals x 7 rows, and enough of them decided per row
    // that the per-row part is what is being compared.
    assert_eq!(compared, sets.len() * readers.len() * rows().len());
    assert!(per_row > sets.len(), "only {per_row} reads were per row");
}

/// Exercise the row projection too: an indirect read must keep the fields
/// that the full request would put on the row's Cedar entity.
fn assert_indirect_read_matches_whole_set(
    resource_type: &str,
    forbid: &str,
    field: &str,
    blocked_id: &str,
    who: &SecurityContext,
) {
    let blocked = HashMap::from([
        ("id".to_string(), json!(blocked_id)),
        ("status".to_string(), json!("Open")),
        (field.to_string(), json!(true)),
    ]);
    let clear = HashMap::from([
        ("id".to_string(), json!("other")),
        ("status".to_string(), json!("Open")),
        (field.to_string(), json!(false)),
    ]);
    // A row-dependent permit reproduces the reported split bypass. A
    // list-wide permit also verifies that uniformity cannot allow all rows.
    for condition in [r#"when { resource.status == "Open" }"#, ""] {
        let engine = AuthzEngine::empty();
        let policy = format!(
            "permit(principal, action == Action::\"read\", resource is {resource_type}) {condition};\n{forbid}"
        );
        engine.reload_tenant_policies("tenant", &policy).unwrap();
        let list = engine.list_read_for_tenant(
            "tenant",
            who,
            "read",
            resource_type,
            &row("_list", None, ""),
        );
        for (attrs, expected) in [(&blocked, false), (&clear, true)] {
            let whole = engine
                .authorize_row_for_tenant("tenant", who, "read", resource_type, attrs)
                .is_allowed();
            assert_eq!(whole, expected, "full request: {policy}");
            let split = match &list {
                ListRead::AllowAll => true,
                ListRead::DenyAll => false,
                ListRead::PerRow(rule) => {
                    let projected = attrs
                        .iter()
                        .filter(|(key, _)| {
                            key.as_str() == "id"
                                || rule
                                    .attribute_keys
                                    .as_ref()
                                    .is_none_or(|keys| keys.contains(*key))
                        })
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect();
                    engine.list_row_allowed("tenant", who, "read", resource_type, rule, &projected)
                }
            };
            assert_eq!(split, whole, "projected split: {policy}, row: {attrs:?}");
        }
        // The rule and any list-wide permit remain tenant-local.
        assert!(matches!(
            engine.list_read_for_tenant("other-tenant", who, "read", resource_type, &blocked,),
            ListRead::DenyAll
        ));
    }
}

#[test]
fn literal_entity_reads_stay_with_the_row_and_keep_its_fields() {
    assert_indirect_read_matches_whole_set(
        "Order",
        r#"forbid(principal, action == Action::"read", resource is Order)
           when { Order::"reader" has blocked && Order::"reader".blocked };"#,
        "blocked",
        "reader",
        &reader(PrincipalKind::Customer, None),
    );
}

#[test]
fn self_row_principal_reads_stay_with_the_row_and_keep_its_fields() {
    assert_indirect_read_matches_whole_set(
        "Customer",
        r#"forbid(principal, action == Action::"read", resource is Customer)
           when { principal has blocked && principal.blocked };"#,
        "blocked",
        "reader",
        &reader(PrincipalKind::Customer, None),
    );
}

#[test]
fn action_entity_reads_stay_with_the_row_and_keep_its_fields() {
    assert_indirect_read_matches_whole_set(
        "Action",
        r#"forbid(principal, action == Action::"read", resource is Action)
           when { action has blocked && action.blocked };"#,
        "blocked",
        "read",
        &reader(PrincipalKind::Customer, None),
    );
}

/// Even the normally type-wide has_spec field can be overwritten by the
/// principal's attributes when resource and principal share a Cedar UID.
#[test]
fn self_row_resource_reads_observe_the_merged_entity() {
    let engine = AuthzEngine::empty();
    engine
        .reload_tenant_policies(
            "tenant",
            r#"
        permit(principal, action == Action::"read", resource is Customer);
        forbid(principal, action == Action::"read", resource is Customer)
        when { resource.has_spec == false };
    "#,
        )
        .unwrap();
    let mut who = reader(PrincipalKind::Customer, None);
    who.principal
        .attributes
        .insert("has_spec".into(), json!(false));
    let probe = HashMap::from([
        ("id".to_string(), json!("_list")),
        ("has_spec".to_string(), json!(true)),
    ]);
    let list = engine.list_read_for_tenant("tenant", &who, "read", "Customer", &probe);
    for (id, expected) in [("reader", false), ("other", true)] {
        let mut attrs = probe.clone();
        attrs.insert("id".to_string(), json!(id));
        let whole = engine
            .authorize_row_for_tenant("tenant", &who, "read", "Customer", &attrs)
            .is_allowed();
        assert_eq!(whole, expected);
        let split = match &list {
            ListRead::AllowAll => true,
            ListRead::DenyAll => false,
            ListRead::PerRow(rule) => {
                engine.list_row_allowed("tenant", &who, "read", "Customer", rule, &attrs)
            }
        };
        assert_eq!(split, whole, "self-row has_spec differs for {id}");
    }
}

#[test]
fn context_values_cedar_omits_do_not_hide_row_fields() {
    for value in [json!(null), json!(1.5), json!({"nested": true})] {
        let mut who = reader(PrincipalKind::Customer, None);
        who.context_attrs.insert("blocked".into(), value);
        assert_indirect_read_matches_whole_set(
            "Order",
            r#"forbid(principal, action == Action::"read", resource is Order)
               when { context has blocked && context.blocked };"#,
            "blocked",
            "reader",
            &who,
        );
    }
}
