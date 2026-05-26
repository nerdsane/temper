#![allow(dead_code)]

include!("../../common.rs");

temper_side_effect_module! {
    fn run(ctx: Context) -> Result<Value> {
        if ctx.trigger_action != "RecordSignal" {
            return Err(format!(
                "signal_observer: unsupported trigger action {}",
                ctx.trigger_action
            ));
        }

        let signal_id = entity_id(&ctx);
        let fields = fields(&ctx);
        let base_url = resolve_api_url(&ctx);
        let headers = odata_headers(&ctx);
        let organism_id = field_str(&fields, &["OrganismId"]);
        let source = field_str(&fields, &["Source"]);
        let signal_kind = field_str(&fields, &["SignalKind"]);
        let summary = field_str(&fields, &["Summary"]);
        let evidence_artifact_id = field_str(&fields, &["EvidenceArtifactId"]);
        let correlation_json = field_str(&fields, &["CorrelationJson"]);

        let work_item_id = create_entity(&ctx, &base_url, &headers, "WorkItems")?;
        let prompt = observer_prompt(
            &signal_id,
            &organism_id,
            &source,
            &signal_kind,
            &summary,
            &evidence_artifact_id,
            &correlation_json,
        );
        post_directed_action(
            &ctx,
            &base_url,
            &headers,
            "WorkItems",
            &work_item_id,
            "QueueWorkItem",
            json!({
                "Role": "observer",
                "TargetEntityType": "Signal",
                "TargetEntityId": signal_id,
                "PromptRef": format!("literal:{prompt}"),
                "ContextRef": format!("signal:{signal_id}"),
                "OutputSchemaRef": "directed-evolution.observer.v1",
                "CorrelationJson": json!({
                    "signal_id": signal_id,
                    "organism_id": organism_id,
                    "source": source,
                    "signal_kind": signal_kind,
                    "evidence_artifact_id": evidence_artifact_id,
                }).to_string(),
            }),
        )?;

        Ok(json!({
            "signal_id": signal_id,
            "observer_work_item_id": work_item_id,
        }))
    }
}

fn observer_prompt(
    signal_id: &str,
    organism_id: &str,
    source: &str,
    signal_kind: &str,
    summary: &str,
    evidence_artifact_id: &str,
    correlation_json: &str,
) -> String {
    format!(
        "Observe this Directed Evolution signal and infer whether it creates actionable pressure.\n\
SignalId: {signal_id}\n\
OrganismId: {organism_id}\n\
Source: {source}\n\
SignalKind: {signal_kind}\n\
Summary: {summary}\n\
EvidenceArtifactId: {evidence_artifact_id}\n\
CorrelationJson: {correlation_json}\n\n\
Return JSON with: actionable, pressure_class, pressure_summary, title, direction_summary, \
autonomy_lane, proposed_adaptation_goal, proposed_viability_constraints, evidence_uri, and rationale. \
If the signal is user error or noise, set actionable=false and explain why."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observer_prompt_names_signal_and_actionability() {
        let prompt = observer_prompt(
            "sig-1",
            "org-1",
            "datadog",
            "latency_regression",
            "p95 climbed",
            "ev-1",
            "{}",
        );

        assert!(prompt.contains("SignalId: sig-1"));
        assert!(prompt.contains("actionable=false"));
        assert!(prompt.contains("proposed_adaptation_goal"));
    }
}
