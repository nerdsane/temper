use super::*;

#[test]
fn caller_authority_binds_the_host_trigger_and_response_budget() {
    let security = SecurityContext::system();
    let identity = BootstrapInvocationIdentity {
        module_name: "module".into(),
        artifact_digest: "artifact".into(),
        grant_digest: "grant".into(),
        trigger: "first".into(),
        max_response_bytes: 4_096,
    };
    let first = caller_authority_digest(&security, &identity).unwrap();
    let mut changed = identity.clone();
    changed.trigger = "second".into();
    assert_ne!(first, caller_authority_digest(&security, &changed).unwrap());
    changed = identity.clone();
    changed.max_response_bytes = 8_192;
    assert_ne!(first, caller_authority_digest(&security, &changed).unwrap());
}
