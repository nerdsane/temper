use super::*;

#[test]
fn issue_title_prefers_intent_shaped_fields() {
    let finding = AgentFinding {
        title: "Invoice entity type not implemented".to_string(),
        symptom_title: "GenerateInvoice hits EntitySetNotFound on Invoice".to_string(),
        intent_title: "Enable invoice generation workflow".to_string(),
        recommended_issue_title: "Enable invoice generation workflow".to_string(),
        intent: "Generate invoices for customers".to_string(),
        ..AgentFinding::default()
    };

    assert_eq!(
        finding_issue_title(&finding),
        "Enable invoice generation workflow"
    );
    assert_eq!(
        finding_symptom_title(&finding),
        "GenerateInvoice hits EntitySetNotFound on Invoice"
    );
    assert_eq!(
        finding_intent_title(&finding),
        "Enable invoice generation workflow"
    );
}
