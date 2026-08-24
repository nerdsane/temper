//! Stable durable-delivery identity derivation.

use sha2::{Digest, Sha256};

/// Derive a length-prefixed immutable identity for one committed delivery.
pub fn stable_delivery_id(
    tenant: &str,
    source_entity_type: &str,
    source_entity_id: &str,
    source_action: &str,
    source_sequence: u64,
    trigger_name: &str,
    trigger_index: usize,
) -> String {
    let mut digest = Sha256::new();
    for component in [
        tenant,
        source_entity_type,
        source_entity_id,
        source_action,
        trigger_name,
    ] {
        digest.update(component.len().to_be_bytes());
        digest.update(component.as_bytes());
    }
    digest.update(source_sequence.to_be_bytes());
    digest.update(trigger_index.to_be_bytes());
    format!("reaction-v1-{:x}", digest.finalize())
}
