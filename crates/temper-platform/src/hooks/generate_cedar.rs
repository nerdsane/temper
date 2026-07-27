use temper_server::ServerState;
use temper_server::authz::{DecisionPolicyReceipt, verify_active_policy_exactly_once};
use temper_server::state::{
    DecisionResolutionKind, DecisionResolutionPhase, DecisionStatus, PendingDecision,
};

/// Verify the canonical, preinstalled decision policy receipt.
///
/// Policy generation, persistence, and activation happen once in the REST
/// approval boundary. This post-commit spec hook deliberately performs no
/// mutation: it proves that the GovernanceDecision fields reproduce the exact
/// generated policy and that precisely one structurally-equal policy is active.
/// A direct `GovernanceDecision.Approve` without that preinstalled receipt
/// therefore fails closed and its later callback effect is not run.
pub(super) fn handle_generate_cedar_from_fields(
    entity_type: &str,
    entity_id: &str,
    fields: &serde_json::Value,
    server: &ServerState,
) -> Result<(), String> {
    if entity_type != "GovernanceDecision" {
        return Err(format!(
            "GenerateCedarPolicy is not valid for entity type {entity_type:?}"
        ));
    }
    let required_string = |name: &str| -> Result<&str, String> {
        fields
            .get(name)
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                format!(
                    "GenerateCedarPolicy: missing required field {name:?} for entity {entity_id:?}"
                )
            })
    };
    let tenant = required_string("tenant")?;
    let agent_id = required_string("agent_id")?;
    let action_name = required_string("action_name")?;
    let resource_type = required_string("resource_type")?;
    // An empty resource id can be legitimate for a type-wide or any-resource
    // scope. Presence and string type are required; non-emptiness is not.
    let resource_id = fields
        .get("resource_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            format!(
                "GenerateCedarPolicy: missing required field {:?} for entity {entity_id:?}",
                "resource_id"
            )
        })?;
    let pending_decision_id = required_string("pending_decision_id")?;
    let encoded_receipt = required_string("scope")?;
    let generated_policy = required_string("generated_policy")?;

    let receipt = DecisionPolicyReceipt::decode(encoded_receipt)?;
    if receipt.pending_decision_id != pending_decision_id {
        return Err(format!(
            "GenerateCedarPolicy: receipt pending decision {:?} does not match entity {:?}",
            receipt.pending_decision_id, pending_decision_id
        ));
    }
    if receipt.governance_decision_id != entity_id {
        return Err(format!(
            "GenerateCedarPolicy: receipt governance decision {:?} does not match entity {:?}",
            receipt.governance_decision_id, entity_id
        ));
    }
    temper_authz::validate_policy_scope_matrix(&receipt.scope_matrix)
        .map_err(|error| format!("GenerateCedarPolicy: invalid receipt scope matrix: {error}"))?;
    let expected_policy = temper_authz::generate_cedar_from_matrix(
        agent_id,
        &receipt.principal_kind,
        action_name,
        resource_type,
        resource_id,
        &receipt.scope_matrix,
    )
    .map_err(|error| format!("GenerateCedarPolicy: failed to reproduce policy: {error}"))?;
    if generated_policy != expected_policy {
        return Err(
            "GenerateCedarPolicy: generated policy does not match the bound approval receipt"
                .to_string(),
        );
    }
    verify_active_policy_exactly_once(server, tenant, generated_policy).map_err(|error| {
        format!("GenerateCedarPolicy: preinstalled policy verification failed: {error}")
    })?;

    tracing::info!(
        entity_id,
        tenant,
        pending_decision_id,
        policy_id = %receipt.policy_id(),
        "GenerateCedarPolicy receipt verified"
    );
    Ok(())
}

/// Verify the receipt against the one durable resolution owner and publication.
///
/// The actor fields and active Cedar set are necessary but not sufficient:
/// both are process-local inputs to this hook. The durable PendingDecision
/// claim proves that the REST approval boundary owns this exact governance
/// actor, and the named snapshot row proves that its policy was committed.
pub(super) async fn handle_generate_cedar_from_fields_durable(
    entity_type: &str,
    entity_id: &str,
    fields: &serde_json::Value,
    server: &ServerState,
) -> Result<(), String> {
    handle_generate_cedar_from_fields(entity_type, entity_id, fields, server)?;

    let field = |name: &str| -> Result<&str, String> {
        fields
            .get(name)
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("GenerateCedarPolicy: missing required field {name:?}"))
    };
    let tenant = field("tenant")?;
    let pending_decision_id = field("pending_decision_id")?;
    let generated_policy = field("generated_policy")?;
    let receipt = DecisionPolicyReceipt::decode(field("scope")?)?;
    let store = server
        .metadata_store_for_tenant(tenant)
        .await
        .ok_or_else(|| "GenerateCedarPolicy: durable metadata is not configured".to_string())?;
    let encoded = store
        .get_pending_decision(pending_decision_id)
        .await
        .map_err(|error| format!("GenerateCedarPolicy: decision lookup failed: {error}"))?
        .ok_or_else(|| {
            format!("GenerateCedarPolicy: pending decision {pending_decision_id:?} does not exist")
        })?;
    let decision: PendingDecision = serde_json::from_str(&encoded)
        .map_err(|error| format!("GenerateCedarPolicy: invalid durable decision: {error}"))?;

    if decision.tenant != tenant
        || decision.id != pending_decision_id
        || decision.governance_decision_id.as_deref() != Some(entity_id)
        || decision.agent_id != field("agent_id")?
        || decision.action != field("action_name")?
        || decision.resource_type != field("resource_type")?
        || decision.resource_id != field("resource_id")?
        || decision.principal_kind.as_deref() != Some(receipt.principal_kind.as_str())
    {
        return Err(
            "GenerateCedarPolicy: durable decision does not match the governance receipt"
                .to_string(),
        );
    }
    if decision.status != DecisionStatus::Pending
        || decision.resolution_kind != Some(DecisionResolutionKind::Approve)
        || decision
            .resolution_owner
            .as_deref()
            .is_none_or(str::is_empty)
        || !matches!(
            decision.resolution_phase,
            Some(
                DecisionResolutionPhase::PolicyPublished
                    | DecisionResolutionPhase::GovernanceDispatched
            )
        )
    {
        return Err(
            "GenerateCedarPolicy: durable decision has no active approval owner/publication"
                .to_string(),
        );
    }
    let owned_version = decision.resolution_policy_version.ok_or_else(|| {
        "GenerateCedarPolicy: durable decision is missing its policy publication version"
            .to_string()
    })?;
    let snapshot = store
        .load_policy_snapshot(tenant)
        .await
        .map_err(|error| format!("GenerateCedarPolicy: policy snapshot lookup failed: {error}"))?;
    if snapshot.version < owned_version {
        return Err(format!(
            "GenerateCedarPolicy: policy snapshot version {} predates owned version {owned_version}",
            snapshot.version
        ));
    }
    let policy_id = receipt.policy_id();
    let mut exact_rows = snapshot
        .rows
        .iter()
        .filter(|row| row.policy_id == policy_id);
    let row = exact_rows.next().ok_or_else(|| {
        format!("GenerateCedarPolicy: durable policy row {policy_id:?} is missing")
    })?;
    if exact_rows.next().is_some() || !row.enabled || row.cedar_text != generated_policy {
        return Err(
            "GenerateCedarPolicy: durable policy row is duplicated, disabled, or changed"
                .to_string(),
        );
    }
    Ok(())
}
