//! A principal record's status reaches Cedar on the principal, and nothing a
//! record holds can stand in for it.
use super::*;
use crate::context::{Principal, SecurityContext};

/// The member check an app writes, in person.
const MEMBER_READS: &str = r#"
    permit(principal, action == Action::"read", resource)
    when { principal has Member && principal.Member == "Active" };
"#;

/// A user, signed in as themselves, whose `Member` record is `status`.
fn user(id: &str, status: &str) -> SecurityContext {
    let mut attributes = HashMap::new();
    attributes.insert("Member".to_string(), serde_json::json!(status));
    SecurityContext {
        principal: Principal {
            id: id.to_string(),
            kind: PrincipalKind::Customer,
            role: None,
            acting_for: None,
            agent_type: None,
            label: None,
            attributes,
        },
        context_attrs: HashMap::new(),
        correlation_id: "principal-record".to_string(),
        cause: None,
        credential: None,
    }
}

fn reads(engine: &AuthzEngine, ctx: &SecurityContext, resource_type: &str, id: &str) -> bool {
    let mut attrs = HashMap::new();
    attrs.insert("id".to_string(), serde_json::json!(id));
    attrs.insert("Member".to_string(), serde_json::json!("Active"));
    engine
        .authorize_for_tenant("tenant", ctx, "read", resource_type, &attrs)
        .is_allowed()
}

/// Reading a record that is the caller's own principal entity (same type and
/// id) merges the two into one Cedar entity. A field of that record named
/// like a principal attribute must not replace the one the identity set.
#[test]
fn a_field_of_the_record_the_principal_reads_does_not_replace_its_status() {
    let engine = AuthzEngine::empty();
    engine
        .reload_tenant_policies("tenant", MEMBER_READS)
        .expect("policy parses");
    let removed = user("alice", "Removed");
    assert!(
        !reads(&engine, &removed, "Customer", "alice"),
        "a removed member reading a record that says Member = Active about itself"
    );
    assert!(
        !reads(&engine, &removed, "Note", "n-1"),
        "a removed member reading a note that says Member = Active"
    );
    assert!(reads(&engine, &user("alice", "Active"), "Note", "n-1"));
}

/// The status is the caller's, the same for every row: a list read decides
/// once for the page.
#[test]
fn a_member_check_is_decided_once_per_list_page() {
    let engine = AuthzEngine::empty();
    engine
        .reload_tenant_policies("tenant", MEMBER_READS)
        .expect("policy parses");
    assert_eq!(
        engine.list_read_uniformity("tenant", &user("alice", "Active"), "read", "Note"),
        ListReadUniformity::Uniform
    );
}
