use super::*;
use temper_spec::csdl::parse_csdl;

const CSDL_XML: &str = include_str!("../../../../test-fixtures/specs/model.csdl.xml");
const ORDER_IOA: &str = include_str!("../../../../test-fixtures/specs/order.ioa.toml");

fn minimal_csdl() -> (CsdlDocument, String) {
    let doc = parse_csdl(CSDL_XML).expect("CSDL should parse");
    (doc, CSDL_XML.to_string())
}

fn unsupported_order_ioa() -> String {
    format!(
        r#"{ORDER_IOA}

[[invariant]]
name = "UnsupportedRegistrySafety"
when = ["Draft"]
assert = "ghost ** quota"
"#
    )
}

#[test]
fn first_registration_rejects_unsupported_safety_invariants_atomically() {
    let mut registry = SpecRegistry::new();
    let (csdl, xml) = minimal_csdl();
    let unsupported = unsupported_order_ioa();

    let error = registry
        .try_register_tenant("alpha", csdl, xml, &[("Order", &unsupported)])
        .expect_err("unsupported safety must not become live");

    assert!(matches!(
        error,
        RegistryError::UnsupportedSafetyInvariants { .. }
    ));
    assert!(registry.get_tenant(&TenantId::new("alpha")).is_none());
}

#[test]
fn hot_swap_rejects_unsupported_safety_before_mutating_live_spec() {
    let mut registry = SpecRegistry::new();
    let (csdl, xml) = minimal_csdl();
    registry.register_tenant("alpha", csdl, xml, &[("Order", ORDER_IOA)]);
    let unsupported = unsupported_order_ioa();
    let (replacement_csdl, replacement_xml) = minimal_csdl();

    let error = registry
        .try_register_tenant(
            "alpha",
            replacement_csdl,
            replacement_xml,
            &[("Order", &unsupported)],
        )
        .expect_err("unsupported hot swap must be rejected");

    assert!(matches!(
        error,
        RegistryError::UnsupportedSafetyInvariants { .. }
    ));
    let current = registry.get_tenant(&TenantId::new("alpha")).unwrap();
    assert_eq!(current.entities["Order"].ioa_source, ORDER_IOA);
}
