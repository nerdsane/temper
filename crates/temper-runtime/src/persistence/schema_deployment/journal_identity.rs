//! Canonical framing for scoped entity journal identities.

use super::{SchemaExecutionPin, SchemaScope, SchemaScopeKind};

/// Return whether a digest uses the canonical lowercase SHA-256 wire form.
pub fn is_canonical_sha256_digest(value: &str) -> bool {
    value.len() == 71
        && value.strip_prefix("sha256:").is_some_and(|hex| {
            hex.bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
}

/// Build the unambiguous durable entity ID for one complete schema pin.
pub fn scoped_journal_entity_id(entity_id: &str, pin: &SchemaExecutionPin) -> String {
    assert!(!entity_id.is_empty(), "scoped entity ID must not be empty");
    assert!(
        !pin.scope.id.is_empty(),
        "schema scope ID must not be empty"
    );
    assert!(
        is_canonical_sha256_digest(&pin.bundle_digest),
        "schema bundle digest must be canonical"
    );
    format!(
        "{}{}",
        scoped_journal_pin_prefix(entity_id, &pin.scope),
        pin.bundle_digest
    )
}

/// Return the canonical journal prefix for one entity and scope.
pub fn scoped_journal_pin_prefix(entity_id: &str, scope: &SchemaScope) -> String {
    assert!(!entity_id.is_empty(), "scoped entity ID must not be empty");
    assert!(!scope.id.is_empty(), "schema scope ID must not be empty");
    format!("{entity_id}{}", scoped_journal_scope_marker(scope))
}

/// Return the canonical journal suffix for one complete schema pin.
pub fn scoped_journal_pin_suffix(pin: &SchemaExecutionPin) -> String {
    assert!(
        !pin.scope.id.is_empty(),
        "schema scope ID must not be empty"
    );
    assert!(
        is_canonical_sha256_digest(&pin.bundle_digest),
        "schema bundle digest must be canonical"
    );
    format!(
        "{}{}",
        scoped_journal_scope_marker(&pin.scope),
        pin.bundle_digest
    )
}

/// Split a scoped journal entity identity into its entity ID and complete pin.
pub fn split_scoped_journal_entity_id(value: &str) -> Option<(&str, SchemaExecutionPin)> {
    let mut parts = value.rsplitn(4, ':');
    let digest_hex = parts.next()?;
    let digest_algorithm = parts.next()?;
    let scope_id_hex = parts.next()?;
    let entity_marker_and_kind = parts.next()?;
    let (entity_with_marker, scope_kind) = entity_marker_and_kind.rsplit_once(':')?;
    let entity_id = entity_with_marker.strip_suffix(":schema")?;
    if entity_id.is_empty() || scope_id_hex.is_empty() {
        return None;
    }
    let bundle_digest = format!("{digest_algorithm}:{digest_hex}");
    if !is_canonical_sha256_digest(&bundle_digest) {
        return None;
    }
    let kind = match scope_kind {
        "task" => SchemaScopeKind::Task,
        _ => return None,
    };
    let scope_id = String::from_utf8(decode_hex(scope_id_hex)?).ok()?;
    if scope_id.is_empty() {
        return None;
    }
    Some((
        entity_id,
        SchemaExecutionPin {
            scope: SchemaScope { kind, id: scope_id },
            bundle_digest,
        },
    ))
}

/// Return whether an entity ID occupies the reserved scoped-journal namespace.
///
/// Global entity IDs may contain colons, but an exact canonical scoped-journal
/// frame is reserved so global and scoped actors can never share one durable
/// persistence identity.
pub fn is_reserved_scoped_journal_entity_id(value: &str) -> bool {
    split_scoped_journal_entity_id(value).is_some()
}

fn scoped_journal_scope_marker(scope: &SchemaScope) -> String {
    let scope_kind = match scope.kind {
        SchemaScopeKind::Task => "task",
    };
    format!(":schema:{scope_kind}:{}:", encode_hex(scope.id.as_bytes()))
}

fn encode_hex(value: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(value.len().saturating_mul(2));
    for byte in value {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return None;
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| Some((decode_hex_digit(pair[0])? << 4) | decode_hex_digit(pair[1])?))
        .collect()
}

fn decode_hex_digit(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_colon_bearing_entity_and_scope_ids() {
        let pin = SchemaExecutionPin {
            scope: SchemaScope {
                kind: SchemaScopeKind::Task,
                id: "task:with:colons".into(),
            },
            bundle_digest: format!("sha256:{}", "b".repeat(64)),
        };
        let nested_entity = format!("entity:schema:{}", "a".repeat(64));
        let journal_id = scoped_journal_entity_id(&nested_entity, &pin);
        assert_eq!(
            split_scoped_journal_entity_id(&journal_id),
            Some((nested_entity.as_str(), pin))
        );
        assert_eq!(
            split_scoped_journal_entity_id("entity:schema:not-a-digest"),
            None
        );
        assert!(is_reserved_scoped_journal_entity_id(&journal_id));
        assert!(!is_reserved_scoped_journal_entity_id(
            "ordinary:colon:bearing:id"
        ));
    }
}
