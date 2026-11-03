//! Decide whether a list read's row decision is the same for every entity.
//!
//! A collection read otherwise evaluates Cedar once per row and emits a span
//! per evaluation. When every policy that can match this principal, action,
//! and entity type ignores per-row state, one evaluation is the whole answer.
//! A policy that names an entity id, a parent, a resource attribute, or a
//! context key a row's field fills stays on the per-row path so a denied row
//! is still removed.

use std::collections::BTreeSet;
use std::str::FromStr;

use cedar_policy::{
    ActionConstraint, EntityTypeName, EntityUid, Policy, PolicySet, PrincipalConstraint,
    ResourceConstraint,
};

use crate::context::{PrincipalKind, SecurityContext, is_cedar_authority_context_key};

use super::typed_uid;

/// How a list read may share one Cedar decision across rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListReadUniformity {
    /// Every row of this entity type receives the same read decision.
    Uniform,
    /// Rows can differ.
    ///
    /// `attribute_keys` lists the resource attributes the applicable policies
    /// read, as `resource.<key>` or as a `context.<key>` the request does
    /// not fill, excluding `has_spec` (constant for the type) and the entity
    /// id.
    /// `None` means the policy shape was not recognized; the caller must
    /// evaluate each full resource.
    PerEntity {
        /// Resource attributes that can change the decision from row to row.
        attribute_keys: Option<BTreeSet<String>>,
    },
}

/// Classify the policies that can match this list read.
pub(super) fn classify(
    policies: &PolicySet,
    security_ctx: &SecurityContext,
    action: &str,
    resource_type: &str,
) -> ListReadUniformity {
    split(policies, security_ctx, action, resource_type)
        .map_or_else(per_entity_unknown, |split| split.uniformity)
}

/// The policies that can match one list read, parted by whether a row can
/// change their result.
///
/// Cedar decides from the set of satisfied policies: a satisfied forbid
/// denies, otherwise a satisfied permit allows. Whether one policy is
/// satisfied never depends on another, so `list` can be evaluated once for
/// the whole read and `row` once per row, and the two results combine into
/// exactly the decision the whole set gives that row.
pub(super) struct ListSplit {
    pub(super) uniformity: ListReadUniformity,
    /// Policies whose result is the same for every row of the read.
    pub(super) list: Vec<Policy>,
    /// Policies whose result can differ from row to row.
    pub(super) row: Vec<Policy>,
}

/// Part the policies that can match this list read; `None` when the request
/// itself cannot be named in Cedar.
pub(super) fn split(
    policies: &PolicySet,
    security_ctx: &SecurityContext,
    action: &str,
    resource_type: &str,
) -> Option<ListSplit> {
    let principal_uid = principal_uid(security_ctx)?;
    let action_uid = typed_uid("Action", action).ok()?;
    let resource_type_name = EntityTypeName::from_str(resource_type).ok()?;

    // Use the engine's conversion: null, objects, and non-integer numbers
    // are omitted, so their keys can still be filled by a row's fields.
    let mut request_context = std::collections::HashMap::new();
    for (key, value) in &security_ctx.context_attrs {
        super::insert_json_as_cedar(&mut request_context, key.clone(), value);
    }
    // Authority keys never come from rows; actual request values take
    // precedence over row values in evaluate_request.
    let request_fills =
        |key: &str| is_cedar_authority_context_key(key) || request_context.contains_key(key);
    let mut varies = false;
    let mut keys = Some(BTreeSet::new());
    let mut list = Vec::new();
    let mut row = Vec::new();
    for policy in policies.policies() {
        if !policy_might_apply(policy, &principal_uid, &action_uid, &resource_type_name) {
            continue;
        }
        let variance = if policy.is_static() {
            row_variance(
                policy,
                &resource_type_name,
                principal_uid.type_name() == &resource_type_name,
                &request_fills,
            )
        } else {
            RowVariance::Unknown
        };
        match variance {
            RowVariance::Uniform => list.push(policy.clone()),
            RowVariance::Varies(policy_keys) => {
                varies = true;
                match (policy_keys, keys.as_mut()) {
                    (Some(policy_keys), Some(keys)) => keys.extend(policy_keys),
                    (Some(_), None) => {}
                    (None, _) => keys = None,
                }
                row.push(policy.clone());
            }
            RowVariance::Unknown => {
                varies = true;
                keys = None;
                row.push(policy.clone());
            }
        }
    }

    let uniformity = if varies {
        ListReadUniformity::PerEntity {
            attribute_keys: keys,
        }
    } else {
        ListReadUniformity::Uniform
    };
    Some(ListSplit {
        uniformity,
        list,
        row,
    })
}

fn per_entity_unknown() -> ListReadUniformity {
    ListReadUniformity::PerEntity {
        attribute_keys: None,
    }
}

enum RowVariance {
    Uniform,
    Varies(Option<BTreeSet<String>>),
    Unknown,
}

/// `request_fills(key)`: whether the request itself fills `context.<key>`.
fn row_variance(
    policy: &Policy,
    resource_type: &EntityTypeName,
    principal_can_be_row: bool,
    request_fills: &dyn Fn(&str) -> bool,
) -> RowVariance {
    let scope_varies = match policy.resource_constraint() {
        ResourceConstraint::Any => false,
        ResourceConstraint::Is(expected) => {
            if &expected != resource_type {
                return RowVariance::Uniform;
            }
            false
        }
        ResourceConstraint::Eq(expected) => {
            if expected.type_name() != resource_type {
                return RowVariance::Uniform;
            }
            true
        }
        ResourceConstraint::In(_) => true,
        ResourceConstraint::IsIn(expected, _) => {
            if &expected != resource_type {
                return RowVariance::Uniform;
            }
            true
        }
    };

    let json = match policy.to_json() {
        Ok(json) => json,
        Err(_) => return RowVariance::Unknown,
    };
    let mut scan = AttrScan::default();
    if let Some(conditions) = json.get("conditions").and_then(|value| value.as_array()) {
        for condition in conditions {
            let body = condition.get("body").unwrap_or(condition);
            // A self-row shares the principal's Cedar UID. evaluate_request
            // merges its fields into that entity, filling attributes the
            // identity did not supply, while identity attributes override
            // resource fields (even has_spec). Keep the full merged entity
            // for conditions reading either alias of it.
            if principal_can_be_row
                && (json_contains_var(body, "principal") || json_contains_var(body, "resource"))
            {
                return RowVariance::Unknown;
            }
            scan.merge(collect_resource_attrs(body));
        }
    } else if json.get("conditions").is_some() {
        return RowVariance::Unknown;
    }

    if scan.unknown
        || scan
            .nested_context_keys
            .iter()
            .any(|key| !request_fills(key))
    {
        return RowVariance::Unknown;
    }
    // The engine puts each resource attribute, the row's fields among them,
    // into the context under its own name unless the request fills that key.
    // So `context.owner` is the row's `owner` field: a page decided once on a
    // probe without fields would skip a `forbid ... when { context.owner !=
    // principal.id }` and show one user everyone's rows.
    scan.keys.extend(
        scan.context_keys
            .into_iter()
            .filter(|key| !request_fills(key)),
    );
    if scope_varies || scan.identity || !scan.keys.is_empty() {
        RowVariance::Varies(Some(scan.keys))
    } else {
        RowVariance::Uniform
    }
}

#[derive(Default)]
struct AttrScan {
    keys: BTreeSet<String>,
    /// Keys the condition reads as `context.<key>`.
    context_keys: BTreeSet<String>,
    /// Keys the condition reads inside, as `context.<key>.<attr>`.
    nested_context_keys: BTreeSet<String>,
    /// The condition names the row's id (`resource.id`, `resource.Id`, or `resource.entity_id`).
    identity: bool,
    /// The condition reads the resource in a shape this walk does not project.
    unknown: bool,
}

impl AttrScan {
    fn merge(&mut self, other: Self) {
        self.keys.extend(other.keys);
        self.context_keys.extend(other.context_keys);
        self.nested_context_keys.extend(other.nested_context_keys);
        self.identity |= other.identity;
        self.unknown |= other.unknown;
    }
}

fn collect_resource_attrs(value: &serde_json::Value) -> AttrScan {
    let mut scan = AttrScan::default();
    collect_resource_attrs_into(value, &mut scan);
    scan
}

fn collect_resource_attrs_into(value: &serde_json::Value, scan: &mut AttrScan) {
    match value {
        serde_json::Value::Array(items) => {
            for item in items {
                collect_resource_attrs_into(item, scan);
            }
        }
        serde_json::Value::Object(map) => {
            if json_var_is(value, "resource") || json_var_is(value, "context") {
                // A bare `resource` (entity equality, a function argument, …)
                // can depend on more than the id, and a bare `context` holds
                // the row's fields. Keep the full document.
                scan.unknown = true;
            }
            if let Some(node) = map.get(".").or_else(|| map.get("has")) {
                match node.get("attr").and_then(|attr| attr.as_str()) {
                    Some(attr) => {
                        let target = node.get("expr").or_else(|| node.get("left"));
                        classify_getattr(attr, target, scan);
                    }
                    None => scan.unknown = true,
                }
                // The getattr target is classified above. Walk the rest of
                // that node (the attribute expression can itself be a call)
                // without treating its `resource` variable as a bare use.
                if let Some(node_map) = node.as_object() {
                    for (key, child) in node_map {
                        if key == "expr" || key == "left" {
                            collect_except_resource_var(child, scan);
                        } else {
                            collect_resource_attrs_into(child, scan);
                        }
                    }
                }
            }
            for (key, child) in map {
                if key == "." || key == "has" {
                    continue;
                }
                collect_resource_attrs_into(child, scan);
            }
        }
        _ => {}
    }
}

/// Walk a getattr target, ignoring the `resource`/`context` variable that
/// the getattr itself already classified.
fn collect_except_resource_var(value: &serde_json::Value, scan: &mut AttrScan) {
    if json_var_is(value, "resource") || json_var_is(value, "context") {
        return;
    }
    collect_resource_attrs_into(value, scan);
}

fn classify_getattr(attr: &str, target: Option<&serde_json::Value>, scan: &mut AttrScan) {
    let Some(target) = target else {
        scan.unknown = true;
        return;
    };
    if json_var_is(target, "resource") {
        if matches!(attr, "id" | "Id" | "entity_id") {
            // The id is already on every row. It still makes the decision
            // per-entity, and it is not a catalog JSON field to project.
            scan.identity = true;
        } else if attr != "has_spec" {
            scan.keys.insert(attr.to_string());
        }
        return;
    }
    if json_var_is(target, "context") {
        // Kept apart: only `row_variance` knows which keys the request fills.
        scan.context_keys.insert(attr.to_string());
        return;
    }
    if let Some(key) = context_root(target) {
        // `context.owner.name`: like a nested resource read when a row
        // fills `owner`.
        scan.nested_context_keys.insert(key.to_string());
        return;
    }
    if json_var_is(target, "principal") {
        // Principal reads on a type that can be the principal's own row
        // were kept per-row above. Other direct request reads are fixed.
        return;
    }
    // An action, entity literal, or expression returning an entity can name
    // the current row too: Order::"secret".blocked sees that field only
    // while checking the secret row. Nested reads also need the full row.
    // Do not infer independence from the absence of a `resource` variable.
    scan.unknown = true;
}

/// The context key a chain of attribute reads starts from: `owner` for
/// `context.owner.name`.
fn context_root(value: &serde_json::Value) -> Option<&str> {
    let node = value.get(".")?;
    let left = node.get("left").or_else(|| node.get("expr"))?;
    if json_var_is(left, "context") {
        return node.get("attr")?.as_str();
    }
    context_root(left)
}

fn json_var_is(value: &serde_json::Value, name: &str) -> bool {
    value.get("Var").and_then(|var| var.as_str()) == Some(name)
}

fn json_contains_var(value: &serde_json::Value, name: &str) -> bool {
    match value {
        serde_json::Value::Array(items) => items.iter().any(|item| json_contains_var(item, name)),
        serde_json::Value::Object(map) => {
            json_var_is(value, name) || map.values().any(|child| json_contains_var(child, name))
        }
        _ => false,
    }
}

fn policy_might_apply(
    policy: &Policy,
    principal_uid: &EntityUid,
    action_uid: &EntityUid,
    resource_type: &EntityTypeName,
) -> bool {
    if !policy.is_static() {
        return true;
    }
    principal_might_match(policy.principal_constraint(), principal_uid)
        && action_might_match(policy.action_constraint(), action_uid)
        && resource_type_might_match(policy.resource_constraint(), resource_type)
}

fn principal_might_match(constraint: PrincipalConstraint, principal_uid: &EntityUid) -> bool {
    match constraint {
        PrincipalConstraint::Any | PrincipalConstraint::In(_) => true,
        PrincipalConstraint::Eq(expected) => expected == *principal_uid,
        PrincipalConstraint::Is(expected) => principal_uid.type_name() == &expected,
        PrincipalConstraint::IsIn(expected, _) => principal_uid.type_name() == &expected,
    }
}

fn action_might_match(constraint: ActionConstraint, action_uid: &EntityUid) -> bool {
    match constraint {
        ActionConstraint::Any => true,
        ActionConstraint::Eq(expected) => expected == *action_uid,
        ActionConstraint::In(expected) => expected.iter().any(|candidate| candidate == action_uid),
    }
}

fn resource_type_might_match(
    constraint: ResourceConstraint,
    resource_type: &EntityTypeName,
) -> bool {
    match constraint {
        ResourceConstraint::Any | ResourceConstraint::In(_) => true,
        ResourceConstraint::Is(expected) | ResourceConstraint::IsIn(expected, _) => {
            &expected == resource_type
        }
        ResourceConstraint::Eq(expected) => expected.type_name() == resource_type,
    }
}

fn principal_uid(security_ctx: &SecurityContext) -> Option<EntityUid> {
    let kind = match security_ctx.principal.kind {
        PrincipalKind::Customer => "Customer",
        PrincipalKind::Agent => "Agent",
        PrincipalKind::Admin => "Admin",
        PrincipalKind::System => "System",
    };
    typed_uid(kind, &security_ctx.principal.id).ok()
}

#[cfg(test)]
#[path = "list_uniformity_test.rs"]
mod tests;
