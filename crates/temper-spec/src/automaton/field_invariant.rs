//! Field invariants — cross-field validation on a single entity instance,
//! checked on writes (ADR-0041). The assertion uses the shared
//! [`crate::predicate`] grammar.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

/// A single cross-field validation rule on one entity, checked on writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldInvariant {
    /// Invariant name (used in error bodies and logs).
    pub name: String,
    /// Must hold for the entity's fields after the write.
    pub assert: crate::predicate::Expr,
    /// Human-readable error message returned on violation. Falls back to a
    /// generic message if omitted.
    #[serde(default)]
    pub message: Option<String>,
}

impl FieldInvariant {
    /// Every field name the assertion reads.
    pub fn referenced_fields(&self) -> BTreeSet<String> {
        let mut fields = BTreeSet::new();
        self.assert.for_each_name(&mut |name| {
            if let crate::predicate::Name::Var(field) = name {
                fields.insert(field.to_string());
            }
        });
        fields
    }

    /// Evaluate this invariant against an entity's fields snapshot.
    pub fn passes(&self, fields: &Json) -> bool {
        let env = crate::predicate::JsonEnv::new(fields);
        crate::predicate::eval(&self.assert, &env) == crate::predicate::Truth::True
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn invariant_passes_when_when_does_not_match() {
        let inv = FieldInvariant {
            name: "LocalMustBeUnrestricted".into(),
            assert: crate::predicate::parse(
                "ConfigType == 'Local' => NetworkingType == 'Unrestricted'",
            )
            .unwrap(),
            message: None,
        };
        assert!(inv.passes(&json!({ "ConfigType": "Cloud" })));
        assert!(inv.passes(&json!({})));
    }

    #[test]
    fn invariant_fails_when_require_does_not_match() {
        let inv = FieldInvariant {
            name: "LocalMustBeUnrestricted".into(),
            assert: crate::predicate::parse(
                "ConfigType == 'Local' => NetworkingType == 'Unrestricted'",
            )
            .unwrap(),
            message: None,
        };
        assert!(inv.passes(&json!({
            "ConfigType": "Local",
            "NetworkingType": "Unrestricted"
        })));
        assert!(!inv.passes(&json!({
            "ConfigType": "Local",
            "NetworkingType": "Limited"
        })));
        assert!(!inv.passes(&json!({ "ConfigType": "Local" })));
    }

    #[test]
    fn field_invariant_deserializes_from_toml() {
        let src = r#"
name = "LocalMustBeUnrestricted"
assert = "ConfigType == 'Local' => NetworkingType == 'Unrestricted'"
message = "Local environments must use Unrestricted networking"
"#;
        let inv: FieldInvariant = toml::from_str(src).expect("should parse");
        assert_eq!(inv.name, "LocalMustBeUnrestricted");
        assert_eq!(
            inv.message.as_deref(),
            Some("Local environments must use Unrestricted networking")
        );
        assert!(inv.passes(&json!({ "ConfigType": "Cloud" })));
        assert!(!inv.passes(&json!({
            "ConfigType": "Local",
            "NetworkingType": "Limited"
        })));
    }
}
