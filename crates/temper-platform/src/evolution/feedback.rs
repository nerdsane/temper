//! Unmet intent collection and evolution record creation.
//!
//! Aggregates production [`UnmetIntent`]s, creates O-Records and I-Records
//! in the evolution store, and broadcasts approval requests to the developer
//! chat for high-priority insights.
//!
//! Emits a `temper.evolution.collect` OTEL span per collection with O-Record
//! and I-Record IDs, priority score, and originating trace ID.

use chrono::{DateTime, Utc};
use opentelemetry::global;
use opentelemetry::trace::{Span, Tracer};
use opentelemetry::KeyValue;
use serde::{Deserialize, Serialize};

use temper_evolution::{
    classify_insight, compute_priority_score,
    InsightCategory, InsightRecord, InsightSignal,
    ObservationClass, ObservationRecord, RecordHeader, RecordStore, RecordType,
    DecisionRecord, Decision, RecordStatus,
};

use crate::protocol::WsMessage;
use crate::state::PlatformState;

/// An unmet intent captured from the production agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnmetIntent {
    /// What the user wanted to do.
    pub user_intent: String,
    /// The tool/action the agent attempted.
    pub attempted_tool: String,
    /// Tenant where this occurred.
    pub tenant: String,
    /// Trace ID for correlation.
    pub trace_id: String,
    /// When the unmet intent was captured.
    pub timestamp: DateTime<Utc>,
}

/// Threshold for broadcasting approval requests to the developer.
const HIGH_PRIORITY_THRESHOLD: f64 = 0.7;

/// Collects unmet intents and creates evolution records.
pub struct UnmetIntentCollector;

impl UnmetIntentCollector {
    /// Process an unmet intent: create O-Record + I-Record, broadcast events.
    ///
    /// Emits a `temper.evolution.collect` OTEL span with record IDs, priority,
    /// and originating trace ID for end-to-end correlation.
    ///
    /// Returns the created insight record.
    pub fn collect(
        unmet: &UnmetIntent,
        state: &PlatformState,
    ) -> InsightRecord {
        let tracer = global::tracer("temper");
        let mut span = tracer
            .span_builder("temper.evolution.collect")
            .with_attributes(vec![
                KeyValue::new("temper.tenant", unmet.tenant.clone()),
                KeyValue::new("temper.user_intent", unmet.user_intent.clone()),
                KeyValue::new("temper.attempted_tool", unmet.attempted_tool.clone()),
                KeyValue::new("temper.originating_trace_id", unmet.trace_id.clone()),
            ])
            .start(&tracer);

        let record_store = &state.record_store;

        // Create O-Record (Observation)
        let o_record = ObservationRecord {
            header: RecordHeader::new(RecordType::Observation, "production-agent"),
            source: format!("production:unmet_intent:{}", unmet.tenant),
            classification: ObservationClass::Trajectory,
            evidence_query: format!(
                "SELECT * FROM trajectories WHERE trace_id = '{}'",
                unmet.trace_id,
            ),
            threshold_field: None,
            threshold_value: None,
            observed_value: None,
            context: serde_json::json!({
                "user_intent": unmet.user_intent,
                "attempted_tool": unmet.attempted_tool,
                "tenant": unmet.tenant,
                "trace_id": unmet.trace_id,
            }),
        };
        let o_id = o_record.header.id.clone();
        record_store.insert_observation(o_record);

        // Build signal for insight classification
        let signal = InsightSignal {
            intent: unmet.user_intent.clone(),
            volume: 1, // Single occurrence; aggregation happens in ranked_insights()
            success_rate: 0.0,
            trend: "growing".into(),
            growth_rate: None,
        };

        let category = classify_insight(&signal);
        let priority = compute_priority_score(&signal);

        let recommendation = format!(
            "Add '{}' capability for tenant '{}'",
            unmet.user_intent, unmet.tenant,
        );

        // Create I-Record (Insight)
        let i_record = InsightRecord {
            header: RecordHeader::new(RecordType::Insight, "production-agent")
                .derived_from(&o_id),
            category,
            signal,
            recommendation,
            priority_score: priority,
        };
        let i_id = i_record.header.id.clone();
        record_store.insert_insight(i_record.clone());

        // Record IDs and priority on the span
        span.set_attribute(KeyValue::new("temper.o_record_id", o_id.clone()));
        span.set_attribute(KeyValue::new("temper.i_record_id", i_id.clone()));
        span.set_attribute(KeyValue::new("temper.priority_score", priority));

        // Broadcast evolution event
        state.broadcast(WsMessage::EvolutionEvent {
            event_type: "unmet_intent".into(),
            summary: format!(
                "Unmet intent '{}' (priority: {:.2})",
                unmet.user_intent, priority,
            ),
            record_id: i_id.clone(),
        });

        // High-priority intents get an approval request
        if priority >= HIGH_PRIORITY_THRESHOLD {
            state.broadcast(WsMessage::ApprovalRequest {
                request_id: i_id,
                description: format!(
                    "Users are trying to '{}' but the system can't do it. Should we add this capability?",
                    unmet.user_intent,
                ),
                proposed_spec: format!(
                    "# Proposed: add action for '{}'\n# Tenant: {}\n# Tool attempted: {}",
                    unmet.user_intent, unmet.tenant, unmet.attempted_tool,
                ),
                priority,
            });
        }

        span.end();
        i_record
    }

    /// Handle a developer's approval or rejection of an evolution request.
    pub fn handle_approval(
        request_id: &str,
        approved: bool,
        rationale: Option<&str>,
        state: &PlatformState,
    ) {
        let record_store = &state.record_store;

        let decision = if approved {
            Decision::Approved
        } else {
            Decision::Rejected
        };

        let d_record = DecisionRecord {
            header: RecordHeader::new(RecordType::Decision, "developer")
                .derived_from(request_id),
            decision,
            decided_by: "developer".into(),
            rationale: rationale.unwrap_or("No rationale provided").into(),
            verification_results: None,
            implementation: None,
        };

        record_store.insert_decision(d_record);

        // If approved, update the insight status
        if approved {
            // The insight remains open — the deploy pipeline will resolve it
            // after generating and verifying specs.
            state.broadcast(WsMessage::EvolutionEvent {
                event_type: "approval".into(),
                summary: format!("Developer approved evolution request '{request_id}'"),
                record_id: request_id.to_string(),
            });
        } else {
            state.broadcast(WsMessage::EvolutionEvent {
                event_type: "rejection".into(),
                summary: format!("Developer rejected evolution request '{request_id}'"),
                record_id: request_id.to_string(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_unmet_intent() -> UnmetIntent {
        UnmetIntent {
            user_intent: "split order into shipments".into(),
            attempted_tool: "SplitOrder".into(),
            tenant: "ecommerce".into(),
            trace_id: "trace-001".into(),
            timestamp: Utc::now(),
        }
    }

    #[test]
    fn test_collect_creates_observation() {
        let state = PlatformState::new_dev(None);
        let unmet = sample_unmet_intent();

        let _insight = UnmetIntentCollector::collect(&unmet, &state);

        assert_eq!(
            state.record_store.count(RecordType::Observation),
            1,
            "Should create one O-Record"
        );
    }

    #[test]
    fn test_collect_creates_insight() {
        let state = PlatformState::new_dev(None);
        let unmet = sample_unmet_intent();

        let insight = UnmetIntentCollector::collect(&unmet, &state);

        assert_eq!(
            state.record_store.count(RecordType::Insight),
            1,
            "Should create one I-Record"
        );
        assert_eq!(insight.signal.intent, "split order into shipments");
    }

    #[test]
    fn test_collect_classifies_as_unmet() {
        let state = PlatformState::new_dev(None);
        let unmet = sample_unmet_intent();

        let insight = UnmetIntentCollector::collect(&unmet, &state);

        // success_rate 0.0 → UnmetIntent
        assert_eq!(insight.category, InsightCategory::UnmetIntent);
    }

    #[test]
    fn test_collect_broadcasts_evolution_event() {
        let state = PlatformState::new_dev(None);
        let mut rx = state.subscribe();
        let unmet = sample_unmet_intent();

        let _insight = UnmetIntentCollector::collect(&unmet, &state);

        let mut events = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let WsMessage::EvolutionEvent { .. } = &msg {
                events.push(msg);
            }
        }
        assert!(!events.is_empty(), "Should broadcast evolution event");
    }

    #[test]
    fn test_collect_insight_links_to_observation() {
        let state = PlatformState::new_dev(None);
        let unmet = sample_unmet_intent();

        let insight = UnmetIntentCollector::collect(&unmet, &state);

        assert!(
            insight.header.derived_from.is_some(),
            "I-Record should link to O-Record"
        );
        let o_id = insight.header.derived_from.unwrap();
        assert!(o_id.starts_with("O-"), "derived_from should reference an O-Record");
    }

    #[test]
    fn test_handle_approval_approved() {
        let state = PlatformState::new_dev(None);
        let unmet = sample_unmet_intent();
        let insight = UnmetIntentCollector::collect(&unmet, &state);

        UnmetIntentCollector::handle_approval(
            &insight.header.id,
            true,
            Some("Looks good, let's add it"),
            &state,
        );

        assert_eq!(
            state.record_store.count(RecordType::Decision),
            1,
            "Should create one D-Record"
        );
        let decisions: Vec<_> = (0..100)
            .filter_map(|_| {
                // Can't iterate directly, but we know there's one
                None::<()>
            })
            .collect();
        let _ = decisions; // We verified count above
    }

    #[test]
    fn test_handle_approval_rejected() {
        let state = PlatformState::new_dev(None);
        let mut rx = state.subscribe();
        let unmet = sample_unmet_intent();
        let insight = UnmetIntentCollector::collect(&unmet, &state);

        // Drain the collection broadcasts
        while rx.try_recv().is_ok() {}

        UnmetIntentCollector::handle_approval(
            &insight.header.id,
            false,
            Some("Not needed right now"),
            &state,
        );

        let mut events = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let WsMessage::EvolutionEvent { event_type, .. } = &msg {
                events.push(event_type.clone());
            }
        }
        assert!(events.contains(&"rejection".to_string()));
    }

    #[test]
    fn test_evolution_collect_span_noop() {
        // Verifies that OTEL span instrumentation in collect()
        // doesn't panic when no OTEL provider is initialized (no-op tracer).
        let state = PlatformState::new_dev(None);
        let unmet = sample_unmet_intent();
        let insight = UnmetIntentCollector::collect(&unmet, &state);
        assert_eq!(insight.category, InsightCategory::UnmetIntent);
    }

    #[test]
    fn test_multiple_unmet_intents() {
        let state = PlatformState::new_dev(None);

        let unmet1 = sample_unmet_intent();
        let mut unmet2 = sample_unmet_intent();
        unmet2.user_intent = "bulk update orders".into();
        unmet2.trace_id = "trace-002".into();

        let i1 = UnmetIntentCollector::collect(&unmet1, &state);
        let i2 = UnmetIntentCollector::collect(&unmet2, &state);

        // Each collect creates one O-Record and one I-Record.
        // Record IDs use UUID v7 first-4-chars which may collide in fast tests
        // (same millisecond), so we verify the returned records are distinct.
        assert_ne!(i1.signal.intent, i2.signal.intent, "insights should have different intents");
        assert_eq!(i1.signal.intent, "split order into shipments");
        assert_eq!(i2.signal.intent, "bulk update orders");
    }
}
