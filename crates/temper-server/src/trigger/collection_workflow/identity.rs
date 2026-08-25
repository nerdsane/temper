//! Domain-separated deterministic identities for collection workflows.

use sha2::{Digest, Sha256};

const WORKFLOW_DOMAIN: &[u8] = b"temper.collection-workflow.v1";
const MEMBER_DOMAIN: &[u8] = b"temper.collection-workflow.member.v1";
const CONTROL_DOMAIN: &[u8] = b"temper.collection-workflow.control.v1";

fn component(digest: &mut Sha256, value: &[u8]) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value);
}

fn finish(prefix: &str, digest: Sha256) -> String {
    format!("{prefix}-{:x}", digest.finalize())
}

/// Derive the immutable identity for one committed workflow start.
pub(crate) fn collection_workflow_id(
    tenant: &str,
    source_entity_type: &str,
    source_entity_id: &str,
    declaration_name: &str,
    source_action: &str,
    source_sequence: u64,
    schema_digest: &str,
) -> String {
    let mut digest = Sha256::new();
    component(&mut digest, WORKFLOW_DOMAIN);
    for value in [
        tenant,
        source_entity_type,
        source_entity_id,
        declaration_name,
        source_action,
        schema_digest,
    ] {
        component(&mut digest, value.as_bytes());
    }
    digest.update(source_sequence.to_be_bytes());
    finish("collection-workflow-v1", digest)
}

/// Derive the immutable identity for one sealed roster member.
pub(crate) fn collection_member_id(
    workflow_id: &str,
    member_index: u32,
    member_value: &str,
) -> String {
    let mut digest = Sha256::new();
    component(&mut digest, MEMBER_DOMAIN);
    component(&mut digest, workflow_id.as_bytes());
    digest.update(member_index.to_be_bytes());
    component(&mut digest, member_value.as_bytes());
    finish("collection-member-v1", digest)
}

/// Return the child identity, which ADR-0181 defines as the member identity.
pub(crate) fn collection_child_id(
    workflow_id: &str,
    member_index: u32,
    member_value: &str,
) -> String {
    collection_member_id(workflow_id, member_index, member_value)
}

/// Derive one stable control-request identity.
pub(crate) fn collection_control_id(
    workflow_id: &str,
    source_action: &str,
    source_sequence: u64,
    requested_outcome: &str,
) -> String {
    let mut digest = Sha256::new();
    component(&mut digest, CONTROL_DOMAIN);
    for value in [workflow_id, source_action, requested_outcome] {
        component(&mut digest, value.as_bytes());
    }
    digest.update(source_sequence.to_be_bytes());
    finish("collection-control-v1", digest)
}
