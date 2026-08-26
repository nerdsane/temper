//! Opaque, tenant-bound keyset cursors for collection workflow reads.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};

const CURSOR_VERSION: u8 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct WorkflowCursor<'a> {
    version: u8,
    tenant: &'a str,
    after_workflow_id: &'a str,
}

#[derive(Debug, Serialize, Deserialize)]
struct MemberCursor<'a> {
    version: u8,
    tenant: &'a str,
    workflow_id: &'a str,
    after_member_index: u32,
}

pub(super) fn encode_workflow(tenant: &str, workflow_id: &str) -> String {
    let payload = serde_json::to_vec(&WorkflowCursor {
        version: CURSOR_VERSION,
        tenant,
        after_workflow_id: workflow_id,
    })
    .expect("workflow cursor serialization is infallible");
    URL_SAFE_NO_PAD.encode(payload)
}

pub(super) fn decode_workflow(cursor: &str, tenant: &str) -> Result<String, ()> {
    let bytes = URL_SAFE_NO_PAD.decode(cursor).map_err(|_| ())?;
    let decoded: WorkflowCursor<'_> = serde_json::from_slice(&bytes).map_err(|_| ())?;
    if decoded.version != CURSOR_VERSION
        || decoded.tenant != tenant
        || decoded.after_workflow_id.is_empty()
    {
        return Err(());
    }
    Ok(decoded.after_workflow_id.to_string())
}

pub(super) fn encode_member(tenant: &str, workflow_id: &str, member_index: u32) -> String {
    let payload = serde_json::to_vec(&MemberCursor {
        version: CURSOR_VERSION,
        tenant,
        workflow_id,
        after_member_index: member_index,
    })
    .expect("member cursor serialization is infallible");
    URL_SAFE_NO_PAD.encode(payload)
}

pub(super) fn decode_member(cursor: &str, tenant: &str, workflow_id: &str) -> Result<u32, ()> {
    let bytes = URL_SAFE_NO_PAD.decode(cursor).map_err(|_| ())?;
    let decoded: MemberCursor<'_> = serde_json::from_slice(&bytes).map_err(|_| ())?;
    if decoded.version != CURSOR_VERSION
        || decoded.tenant != tenant
        || decoded.workflow_id != workflow_id
    {
        return Err(());
    }
    Ok(decoded.after_member_index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_cursor_is_tenant_bound_and_rejects_malformed_input() {
        let cursor = encode_workflow("tenant-a", "workflow-1");
        assert_eq!(
            decode_workflow(&cursor, "tenant-a"),
            Ok("workflow-1".into())
        );
        assert_eq!(decode_workflow(&cursor, "tenant-b"), Err(()));
        assert_eq!(decode_workflow("not-a-cursor", "tenant-a"), Err(()));
    }

    #[test]
    fn member_cursor_is_workflow_bound() {
        let cursor = encode_member("tenant-a", "workflow-1", 7);
        assert_eq!(decode_member(&cursor, "tenant-a", "workflow-1"), Ok(7));
        assert_eq!(decode_member(&cursor, "tenant-a", "workflow-2"), Err(()));
    }
}
