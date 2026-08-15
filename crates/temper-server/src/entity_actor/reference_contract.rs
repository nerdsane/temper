//! Immutable typed-reference and deterministic-identity enforcement (ADR-0156).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use temper_jit::table::TransitionTable;

use super::types::EntityState;

/// Stable typed-reference failure categories exposed at transport boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReferenceViolationCategory {
    /// A reference was missing, empty where required, or not a string.
    InvalidReferenceValue,
    /// The declared same-tenant target did not durably exist.
    ReferenceTargetMissing,
    /// A set reference was cleared or changed.
    ImmutableReferenceViolation,
    /// An incoming reference did not equal the stored reference required by a guard.
    ReferenceEqualityViolation,
    /// A deterministic identity key was missing one or more reference components.
    DeterministicIdIncomplete,
    /// A supplied or routed entity ID did not match the canonical identity hash.
    DeterministicIdMismatch,
}

/// A structured ADR-0156 contract failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceContractViolation {
    /// Stable public failure category.
    pub category: ReferenceViolationCategory,
    /// Entity type whose contract rejected the operation.
    pub entity_type: Box<str>,
    /// Action or write operation being validated.
    pub operation: Box<str>,
    /// Reference, guard, or identity key that failed.
    pub subject: Box<str>,
    /// Declared target, stored value, or derived identity expected by the contract.
    pub expected: Option<Box<str>>,
    /// Caller-supplied value that violated the contract, when present.
    pub supplied: Option<Box<str>>,
}

impl std::fmt::Display for ReferenceContractViolation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "ReferenceContractViolation:{:?}:entity={}:operation={}:subject={}",
            self.category, self.entity_type, self.operation, self.subject
        )?;
        if let Some(expected) = &self.expected {
            write!(formatter, ":expected={expected}")?;
        }
        if let Some(supplied) = &self.supplied {
            write!(formatter, ":supplied={supplied}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ReferenceContractViolation {}

/// Evidence-map key used to pass a durable same-tenant existence result into an actor.
pub fn target_evidence_key(entity_type: &str, entity_id: &str) -> String {
    format!("__ref_exists:{entity_type}:{entity_id}")
}

fn violation(
    table: &TransitionTable,
    operation: &str,
    subject: &str,
    category: ReferenceViolationCategory,
    expected: Option<String>,
    supplied: Option<String>,
) -> ReferenceContractViolation {
    ReferenceContractViolation {
        category,
        entity_type: table.entity_name.clone().into_boxed_str(),
        operation: operation.into(),
        subject: subject.into(),
        expected: expected.map(String::into_boxed_str),
        supplied: supplied.map(String::into_boxed_str),
    }
}

fn field<'a>(
    table: &TransitionTable,
    operation: &str,
    fields: &'a serde_json::Map<String, serde_json::Value>,
    name: &str,
) -> Result<Option<&'a serde_json::Value>, ReferenceContractViolation> {
    let canonical = temper_spec::to_snake_case(name);
    let mut matches = fields
        .iter()
        .filter(|(candidate, _)| temper_spec::to_snake_case(candidate) == canonical);
    let Some((_, first)) = matches.next() else {
        return Ok(None);
    };
    if matches.any(|(_, value)| value != first) {
        return Err(violation(
            table,
            operation,
            name,
            ReferenceViolationCategory::InvalidReferenceValue,
            Some("one unambiguous alias-equivalent field value".into()),
            None,
        ));
    }
    Ok(Some(first))
}

fn reference_value<'a>(
    table: &TransitionTable,
    operation: &str,
    subject: &str,
    value: Option<&'a serde_json::Value>,
) -> Result<Option<&'a str>, ReferenceContractViolation> {
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(value)) if value.is_empty() => Ok(None),
        Some(serde_json::Value::String(value)) => Ok(Some(value)),
        Some(value) => Err(violation(
            table,
            operation,
            subject,
            ReferenceViolationCategory::InvalidReferenceValue,
            Some("non-empty string reference or unset".into()),
            Some(value.to_string()),
        )),
    }
}

/// Derive an identity-key entity ID, or validate a caller-supplied ID.
pub fn derive_or_validate_entity_id(
    table: &TransitionTable,
    supplied_id: Option<&str>,
    fields: &serde_json::Map<String, serde_json::Value>,
    operation: &str,
) -> Result<Option<String>, ReferenceContractViolation> {
    let Some(key) = table.keys.iter().find(|key| key.entity_id) else {
        return Ok(supplied_id.map(str::to_string));
    };
    for property in &key.properties {
        if reference_value(
            table,
            operation,
            property,
            field(table, operation, fields, property)?,
        )?
        .is_none()
        {
            return Err(violation(
                table,
                operation,
                &key.name,
                ReferenceViolationCategory::DeterministicIdIncomplete,
                Some(key.properties.join(",")),
                None,
            ));
        }
    }
    let derived = crate::key_index::canonical_key_hash(&key.name, &key.properties, fields)
        .expect("complete scalar identity key has a canonical hash"); // ci-ok: prechecked above
    if let Some(supplied) = supplied_id
        && supplied != derived
    {
        return Err(violation(
            table,
            operation,
            &key.name,
            ReferenceViolationCategory::DeterministicIdMismatch,
            Some(derived),
            Some(supplied.to_string()),
        ));
    }
    Ok(Some(derived))
}

fn require_target(
    table: &TransitionTable,
    operation: &str,
    subject: &str,
    target_type: &str,
    target_id: &str,
    evidence: &BTreeMap<String, bool>,
) -> Result<(), ReferenceContractViolation> {
    if evidence
        .get(&target_evidence_key(target_type, target_id))
        .copied()
        .unwrap_or(false)
    {
        return Ok(());
    }
    Err(violation(
        table,
        operation,
        subject,
        ReferenceViolationCategory::ReferenceTargetMissing,
        Some(target_type.to_string()),
        Some(target_id.to_string()),
    ))
}

/// Validate typed action parameters before guard evaluation.
pub fn validate_action_parameters(
    table: &TransitionTable,
    action: &str,
    params: &serde_json::Value,
    evidence: &BTreeMap<String, bool>,
) -> Result<(), ReferenceContractViolation> {
    let Some(declarations) = table.action_params.get(action) else {
        return Ok(());
    };
    for (name, declaration) in declarations {
        if declaration.param_type != "ref" {
            continue;
        }
        let value = reference_value(table, action, name, params.get(name))?.ok_or_else(|| {
            violation(
                table,
                action,
                name,
                ReferenceViolationCategory::InvalidReferenceValue,
                Some("non-empty string reference".into()),
                None,
            )
        })?;
        require_target(
            table,
            action,
            name,
            declaration.entity_type.as_deref().unwrap_or_default(),
            value,
            evidence,
        )?;
    }
    Ok(())
}

/// Validate immutable references and deterministic identity on a prospective state.
pub fn validate_prospective_state(
    table: &TransitionTable,
    operation: &str,
    current: &EntityState,
    prospective: &EntityState,
    evidence: &BTreeMap<String, bool>,
) -> Result<(), ReferenceContractViolation> {
    let current_fields = current.fields.as_object().cloned().unwrap_or_default();
    let prospective_fields = prospective.fields.as_object().cloned().unwrap_or_default();
    for (name, metadata) in &table.state_var_metadata {
        if metadata.var_type.as_deref() != Some("ref") {
            continue;
        }
        let before = reference_value(
            table,
            operation,
            name,
            field(table, operation, &current_fields, name)?,
        )?;
        let after = reference_value(
            table,
            operation,
            name,
            field(table, operation, &prospective_fields, name)?,
        )?;
        if let Some(before) = before
            && after != Some(before)
        {
            return Err(violation(
                table,
                operation,
                name,
                ReferenceViolationCategory::ImmutableReferenceViolation,
                Some(before.to_string()),
                after.map(str::to_string),
            ));
        }
        if let Some(after) = after {
            require_target(
                table,
                operation,
                name,
                metadata.entity_type.as_deref().unwrap_or_default(),
                after,
                evidence,
            )?;
        }
    }
    let derived = derive_or_validate_entity_id(
        table,
        Some(&prospective.entity_id),
        &prospective_fields,
        operation,
    )?;
    debug_assert!(derived.is_some() || !table.keys.iter().any(|key| key.entity_id));
    Ok(())
}

/// Convert a failed equality guard to its stable structured category.
pub fn equality_violation(
    table: &TransitionTable,
    operation: &str,
    subject: &str,
    expected: Option<String>,
    supplied: Option<String>,
) -> ReferenceContractViolation {
    violation(
        table,
        operation,
        subject,
        ReferenceViolationCategory::ReferenceEqualityViolation,
        expected,
        supplied,
    )
}

/// Return whether an encoded actor error is an ADR-0156 contract violation.
pub fn is_reference_contract_error(error: &str) -> bool {
    error.starts_with("ReferenceContractViolation:")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC: &str = r#"
[automaton]
name = "Document"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]

[[state]]
name = "workspace_id"
type = "ref"
entity_type = "Workspace"
initial = ""

[[action]]
name = "Attach"
kind = "input"
from = ["Active"]
to = "Active"
params = [{ name = "workspace_id", type = "ref", entity_type = "Workspace" }]
"#;

    fn state(fields: serde_json::Value) -> EntityState {
        EntityState {
            entity_type: "Document".into(),
            entity_id: "doc-1".into(),
            status: "Active".into(),
            item_count: 0,
            counters: BTreeMap::new(),
            booleans: BTreeMap::new(),
            lists: BTreeMap::new(),
            fields,
            events: std::collections::VecDeque::new(),
            total_event_count: 0,
            events_since_snapshot: 0,
            last_snapshot_sequence_nr: 0,
            sequence_nr: 0,
            processed_idempotency_keys: BTreeMap::new(),
        }
    }

    #[test]
    fn rejected_reference_action_is_side_effect_free() {
        let table = TransitionTable::from_ioa_source(SPEC);
        let mut entity = state(serde_json::json!({}));
        let before = serde_json::to_value(&entity).unwrap();
        let result = super::super::effects::process_action_with_xref(
            &mut entity,
            &table,
            "Attach",
            &serde_json::json!({"workspace_id": "missing"}),
            &BTreeMap::new(),
        );
        assert!(!result.success);
        assert!(result.event.is_none());
        assert!(result.custom_effects.is_empty());
        assert!(result.spawn_requests.is_empty());
        assert_eq!(serde_json::to_value(&entity).unwrap(), before);
    }

    #[test]
    fn reference_is_set_once_and_same_value_is_idempotent() {
        let table = TransitionTable::from_ioa_source(SPEC);
        let mut entity = state(serde_json::json!({}));
        let evidence = BTreeMap::from([
            (target_evidence_key("Workspace", "ws-1"), true),
            (target_evidence_key("Workspace", "ws-2"), true),
        ]);
        assert!(
            super::super::effects::process_action_with_xref(
                &mut entity,
                &table,
                "Attach",
                &serde_json::json!({"workspace_id": "ws-1"}),
                &evidence,
            )
            .success
        );
        let after_first = entity.clone();
        assert!(
            super::super::effects::process_action_with_xref(
                &mut entity,
                &table,
                "Attach",
                &serde_json::json!({"workspace_id": "ws-1"}),
                &evidence,
            )
            .success
        );
        let rejected = super::super::effects::process_action_with_xref(
            &mut entity,
            &table,
            "Attach",
            &serde_json::json!({"workspace_id": "ws-2"}),
            &evidence,
        );
        assert!(!rejected.success);
        assert!(
            rejected
                .error
                .unwrap()
                .contains("ImmutableReferenceViolation")
        );
        assert_eq!(entity.fields, after_first.fields);
    }

    #[test]
    fn conflicting_alias_equivalent_reference_fields_are_rejected() {
        let table = TransitionTable::from_ioa_source(SPEC);
        let current = state(serde_json::json!({"workspace_id": "ws-1"}));
        let prospective = state(serde_json::json!({
            "workspace_id": "ws-1",
            "WorkspaceId": "ws-2"
        }));
        let evidence = BTreeMap::from([
            (target_evidence_key("Workspace", "ws-1"), true),
            (target_evidence_key("Workspace", "ws-2"), true),
        ]);
        let error =
            validate_prospective_state(&table, "UpdateFields", &current, &prospective, &evidence)
                .expect_err("conflicting aliases must not hide a rebind");
        assert_eq!(
            error.category,
            ReferenceViolationCategory::InvalidReferenceValue
        );
    }

    #[test]
    fn deterministic_identity_uses_canonical_key_hash() {
        let table = TransitionTable::from_ioa_source(&format!(
            "{SPEC}\n[[key]]\nname = \"workspace\"\nproperties = [\"workspace_id\"]\nentity_id = true\n"
        ));
        let fields = serde_json::json!({"workspace_id": "ws-1"});
        let object = fields.as_object().unwrap();
        let expected =
            crate::key_index::canonical_key_hash("workspace", &["workspace_id".into()], object);
        assert_eq!(
            derive_or_validate_entity_id(&table, None, object, "Create").unwrap(),
            expected
        );
        let error =
            derive_or_validate_entity_id(&table, Some("caller-id"), object, "Create").unwrap_err();
        assert_eq!(
            error.category,
            ReferenceViolationCategory::DeterministicIdMismatch
        );
    }
}
