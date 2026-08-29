//! Pure helpers for Temper collection-workflow contracts.

use sha2::{Digest, Sha256};

const MEMBER_DOMAIN: &[u8] = b"temper.collection-workflow.member.v1";

fn component(digest: &mut Sha256, value: &[u8]) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value);
}

/// Derive the immutable v1 identity for one member of a sealed collection roster.
pub fn collection_member_id_v1(workflow_id: &str, member_index: u32, member_value: &str) -> String {
    let mut digest = Sha256::new();
    component(&mut digest, MEMBER_DOMAIN);
    component(&mut digest, workflow_id.as_bytes());
    digest.update(member_index.to_be_bytes());
    component(&mut digest, member_value.as_bytes());
    format!("collection-member-v1-{:x}", digest.finalize())
}

#[cfg(test)]
mod tests {
    use super::collection_member_id_v1;

    #[test]
    fn member_identity_v1_matches_the_server_golden_vector() {
        assert_eq!(
            collection_member_id_v1(
                "collection-workflow-v1-12f373322f0531282b4b933dd901c1075a9997e1b45066f44e7cb022f579576a",
                3,
                "check-雪"
            ),
            "collection-member-v1-3a8882ea22f287ee78f1b90ba93b8520a87361aba8b3c325227694db8d38ab68"
        );
    }
}
