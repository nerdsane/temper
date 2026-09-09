use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;

use crate::request_context::AgentContext;
use crate::state::ServerState;

use super::model::{
    AgentFinding, build_issue_description, default_acceptance_criteria, finding_issue_title,
    issue_priority_level,
};

pub(super) async fn create_issue_for_finding(
    state: &ServerState,
    tenant: &TenantId,
    summary: &str,
    finding: &AgentFinding,
    record_ids: &[String],
) -> Result<String, String> {
    let issue_id = temper_runtime::scheduler::sim_uuid().to_string();
    let now = sim_now().to_rfc3339();
    let issue_title = finding_issue_title(finding);
    let description = build_issue_description(summary, finding, record_ids);
    let acceptance_criteria = default_acceptance_criteria(finding).join("\n");

    state
        .get_or_create_tenant_entity(
            tenant,
            "Issue",
            &issue_id,
            serde_json::json!({
                "Id": issue_id.clone(),
                "Title": issue_title,
                "Description": description,
                "AcceptanceCriteria": acceptance_criteria,
                "Priority": issue_priority_level(finding.priority_score),
                "CreatedAt": now,
                "UpdatedAt": now,
            }),
        )
        .await?;

    // Walk the issue into Todo. The issue itself is already persisted, so a
    // failed transition leaves it in an earlier state rather than failing
    // materialization — but it must be visible, not silently swallowed.
    let system_ctx = AgentContext::for_service("evolution-engine");
    let setup_actions = [
        (
            "SetPriority",
            serde_json::json!({ "level": issue_priority_level(finding.priority_score) }),
        ),
        ("MoveToTriage", serde_json::json!({})),
        ("MoveToTodo", serde_json::json!({})),
    ];
    for (action, params) in setup_actions {
        if let Err(error) = state
            .dispatch_tenant_action(tenant, "Issue", &issue_id, action, params, &system_ctx)
            .await
        {
            tracing::warn!(
                issue_id = %issue_id,
                action,
                error = %error,
                "issue setup transition failed; issue left in earlier state"
            );
        }
    }

    Ok(issue_id)
}
