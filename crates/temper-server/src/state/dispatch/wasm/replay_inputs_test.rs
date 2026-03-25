use super::*;

#[test]
fn extract_ots_actions_from_choice_arguments() {
    let ots = serde_json::json!({
        "turns": [{
            "decisions": [{
                "choice": {
                    "arguments": {
                        "trajectory_actions": [
                            {"action": "PromoteToCritical", "params": {"Reason": "prod"}},
                            {"action": "Assign", "params": {"AgentId": "agent-2"}}
                        ]
                    }
                }
            }]
        }]
    });

    let actions = extract_trajectory_actions_from_ots(&ots);
    assert_eq!(actions.len(), 2);
    assert_eq!(
        actions[0].get("action").and_then(Value::as_str),
        Some("PromoteToCritical")
    );
}

#[test]
fn extract_ots_actions_from_user_code_message() {
    let ots = serde_json::json!({
        "turns": [{
            "messages": [{
                "role": "user",
                "content": {
                    "text": "temper.action('tenant-1', 'Issues', '11111111-1111-1111-1111-111111111111', 'Reassign', {'NewAssigneeId': 'agent-3'})"
                }
            }]
        }]
    });

    let actions = extract_trajectory_actions_from_ots(&ots);
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0]["action"], serde_json::json!("Reassign"));
    assert_eq!(
        actions[0]["params"]["NewAssigneeId"],
        serde_json::json!("agent-3")
    );
}
