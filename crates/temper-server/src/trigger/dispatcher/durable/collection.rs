//! Durable collection descendant quiescence.

use super::{ReactionDispatcher, ReactionRule};
use temper_runtime::tenant::TenantId;

fn descendant_success_quiescence(
    status: crate::trigger::delivery::ReactionDeliveryStatus,
    last_error: Option<String>,
) -> Result<(), String> {
    use crate::trigger::delivery::ReactionDeliveryStatus;

    match status {
        ReactionDeliveryStatus::Succeeded | ReactionDeliveryStatus::Skipped => Ok(()),
        ReactionDeliveryStatus::Pending
        | ReactionDeliveryStatus::Claimed
        | ReactionDeliveryStatus::Dispatching => {
            Err("collection descendant delivery deferred".to_string())
        }
        ReactionDeliveryStatus::DroppedAllowed
        | ReactionDeliveryStatus::Rejected
        | ReactionDeliveryStatus::DeadLettered => Err(last_error
            .unwrap_or_else(|| "collection descendant delivery failed permanently".to_string())),
    }
}

fn descendant_terminal_quiescence(
    status: crate::trigger::delivery::ReactionDeliveryStatus,
) -> Result<(), String> {
    use crate::trigger::delivery::ReactionDeliveryStatus;

    match status {
        ReactionDeliveryStatus::Pending
        | ReactionDeliveryStatus::Claimed
        | ReactionDeliveryStatus::Dispatching => {
            Err("collection descendant delivery deferred".to_string())
        }
        ReactionDeliveryStatus::Succeeded
        | ReactionDeliveryStatus::Skipped
        | ReactionDeliveryStatus::DroppedAllowed
        | ReactionDeliveryStatus::Rejected
        | ReactionDeliveryStatus::DeadLettered => Ok(()),
    }
}

impl ReactionDispatcher {
    pub(super) async fn quiesce_controlled_member_descendants(
        &self,
        state: &crate::ServerState,
        store: &crate::storage::BoxedEventStore,
        cancellation: &crate::trigger::delivery::PersistedReactionIntent,
    ) -> Result<(), String> {
        let context = cancellation
            .collection
            .as_ref()
            .ok_or_else(|| "collection cancellation context is absent".to_string())?;
        let member_id = context
            .member_id
            .as_deref()
            .ok_or_else(|| "collection cancellation member identity is absent".to_string())?;
        let (workflow, _) = crate::trigger::collection_workflow::load_collection_record(
            store,
            &cancellation.tenant,
            &context.workflow_id,
        )
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "collection workflow journal is missing".to_string())?;
        let member = workflow
            .members
            .iter()
            .find(|member| member.member_id == member_id)
            .ok_or_else(|| "collection cancellation member is missing".to_string())?;
        let member_delivery_id = member
            .delivery_id
            .as_deref()
            .ok_or_else(|| "collection member delivery identity is absent".to_string())?;
        let member_intent = crate::trigger::collection_workflow::find_collection_intent(
            store,
            &workflow,
            member_delivery_id,
        )
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "collection member intent is missing".to_string())?;
        let target_entity_id = member_intent
            .target_entity_id
            .as_deref()
            .ok_or_else(|| "collection member target identity is absent".to_string())?;
        let member_rule: ReactionRule = serde_json::from_value(member_intent.rule.clone())
            .map_err(|error| format!("invalid collection member rule: {error}"))?;
        let target_journal_id = member_intent.schema_pin.as_ref().map_or_else(
            || {
                format!(
                    "{}:{}:{}",
                    member_intent.tenant, member_rule.then.entity_type, target_entity_id
                )
            },
            |pin| {
                format!(
                    "{}:{}:{}",
                    member_intent.tenant,
                    member_rule.then.entity_type,
                    temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
                        target_entity_id,
                        &pin.execution,
                    )
                )
            },
        );
        let target_events = store
            .read_latest_events(
                &target_journal_id,
                crate::entity_actor::types::MAX_DURABLE_IDEMPOTENCY_KEYS_PER_ENTITY,
            )
            .await
            .map_err(|error| error.to_string())?;
        let target_event = target_events
            .iter()
            .find(|event| {
                crate::trigger::delivery::extract_receipt(&event.payload)
                    .ok()
                    .flatten()
                    .is_some_and(|receipt| receipt.delivery_id == member_delivery_id)
            })
            .ok_or_else(|| "collection member target receipt is missing".to_string())?;
        let tenant = TenantId::new(&member_intent.tenant);
        let descendants = state
            .materialize_committed_reaction_intents(
                &tenant,
                &member_rule.then.entity_type,
                target_entity_id,
                target_event.sequence_nr,
                member_intent.schema_pin.as_ref().map(|pin| &pin.execution),
            )
            .await
            .map_err(|error| error.to_string())?;
        self.dispatch_collection_descendants_to_terminal(state, descendants)
            .await
    }

    pub(crate) async fn dispatch_collection_descendants(
        &self,
        state: &crate::ServerState,
        mut descendants: Vec<crate::trigger::delivery::PersistedReactionIntent>,
    ) -> Result<(), String> {
        descendants.sort_by(|left, right| left.delivery_id.cmp(&right.delivery_id));
        for descendant in descendants {
            Box::pin(self.dispatch_committed_intent(state, descendant.clone())).await?;
            let (store, _) = state.event_journal().ok_or_else(|| {
                "durable collection descendant requires an event journal".to_string()
            })?;
            let (record, _) = crate::trigger::delivery::load_delivery_record(&store, descendant)
                .await
                .map_err(|error| error.to_string())?;
            descendant_success_quiescence(record.status, record.last_error)?;
        }
        Ok(())
    }

    async fn dispatch_collection_descendants_to_terminal(
        &self,
        state: &crate::ServerState,
        mut descendants: Vec<crate::trigger::delivery::PersistedReactionIntent>,
    ) -> Result<(), String> {
        descendants.sort_by(|left, right| left.delivery_id.cmp(&right.delivery_id));
        for descendant in descendants {
            Box::pin(self.dispatch_committed_intent(state, descendant.clone())).await?;
            let (store, _) = state.event_journal().ok_or_else(|| {
                "durable collection descendant requires an event journal".to_string()
            })?;
            let (record, _) = crate::trigger::delivery::load_delivery_record(&store, descendant)
                .await
                .map_err(|error| error.to_string())?;
            descendant_terminal_quiescence(record.status)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::ReactionDispatcher;
    use super::{descendant_success_quiescence, descendant_terminal_quiescence};
    use crate::trigger::delivery::ReactionDeliveryStatus;

    #[test]
    fn descendant_quiescence_distinguishes_deferred_skipped_and_permanent() {
        assert!(descendant_success_quiescence(ReactionDeliveryStatus::Succeeded, None).is_ok());
        assert!(descendant_success_quiescence(ReactionDeliveryStatus::Skipped, None).is_ok());
        assert_eq!(
            descendant_success_quiescence(ReactionDeliveryStatus::Pending, None).unwrap_err(),
            "collection descendant delivery deferred"
        );
        assert_eq!(
            descendant_success_quiescence(
                ReactionDeliveryStatus::Rejected,
                Some("permanent".to_string())
            )
            .unwrap_err(),
            "permanent"
        );
        assert!(descendant_terminal_quiescence(ReactionDeliveryStatus::Rejected).is_ok());
        assert!(descendant_terminal_quiescence(ReactionDeliveryStatus::DeadLettered).is_ok());
        assert_eq!(
            descendant_terminal_quiescence(ReactionDeliveryStatus::Dispatching).unwrap_err(),
            "collection descendant delivery deferred"
        );
    }

    #[tokio::test]
    async fn control_before_descendant_redrive_persists_skip_without_join() {
        use std::sync::Arc;

        use temper_runtime::ActorSystem;
        use temper_runtime::persistence::schema_deployment::SchemaEventPin;

        use crate::registry::SpecRegistry;
        use crate::storage::{BoxedEventStore, StorageStack};
        use crate::trigger::collection_workflow::*;
        use crate::trigger::delivery::{
            append_delivery_record, delivery_journal_id, load_delivery_record,
        };
        use crate::trigger::registry::ReactionRegistry;

        const CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices><Schema Namespace="Test" xmlns="http://docs.oasis-open.org/odata/ns/edm">
    <EntityType Name="CheckRun"><Key><PropertyRef Name="Id"/></Key>
      <Property Name="Id" Type="Edm.String" Nullable="false"/>
      <Property Name="Status" Type="Edm.String"/>
    </EntityType>
    <EntityContainer Name="Container"><EntitySet Name="CheckRuns" EntityType="Test.CheckRun"/></EntityContainer>
  </Schema></edmx:DataServices>
</edmx:Edmx>"#;
        const IOA: &str = r#"
[automaton]
name = "CheckRun"
states = ["Pending", "Started"]
initial = "Pending"

[[action]]
name = "Start"
from = ["Pending"]
to = "Started"
"#;

        let tenant_name = "collection-descendant-redrive";
        let mut registry = SpecRegistry::new();
        registry
            .try_register_tenant(
                tenant_name,
                temper_spec::csdl::parse_csdl(CSDL).unwrap(),
                CSDL.to_string(),
                &[("CheckRun", IOA)],
            )
            .unwrap();
        let sim = temper_store_sim::SimEventStore::no_faults(942);
        let store = BoxedEventStore::new(sim.clone());
        let mut state = crate::ServerState::from_registry(
            ActorSystem::new("collection-descendant-redrive"),
            registry,
        );
        state.set_storage_stack(StorageStack::from_sim(sim.clone(), None));
        state
            .authz
            .reload_tenant_policies(tenant_name, "permit(principal, action, resource);")
            .unwrap();

        let authority = serde_json::to_value(
            temper_authz::SecurityContext::from_resolved_identity("test", "system", None),
        )
        .unwrap();
        let (_, mut workflow) = CollectionWorkflowRecordV1::start(CollectionWorkflowStart {
            tenant: tenant_name.to_string(),
            source_entity_type: "Batch".to_string(),
            source_entity_id: "batch-1".to_string(),
            declaration_name: "checks".to_string(),
            source_action: "StartChecks".to_string(),
            source_sequence: 1,
            schema_digest: "sha256:test".to_string(),
            schema_pin: None::<SchemaEventPin>,
            authority: authority.clone(),
            roster: vec!["a".to_string()],
            budgets: CollectionWorkflowBudgets {
                max_members: 1,
                max_concurrency: 1,
                max_attempts: 2,
            },
        })
        .unwrap();
        let actions = CollectionExecutionActions {
            member_entity: "CheckRun",
            member_action: "Start",
            member_cancel_action: "Start",
            timeout_action: "TimeoutChecks",
            on_success: "Joined",
            on_partial_failure: "Joined",
            on_failure: "Joined",
            on_cancelled: "Joined",
            on_timed_out: "Joined",
        };
        let mut descendant = activate_start(&mut workflow, 0, &actions)
            .unwrap()
            .remove(0);
        let member_id = workflow.members[0].member_id.clone();
        workflow
            .record_member_receipt(
                &member_id,
                &descendant.delivery_id,
                0,
                1,
                CollectionMemberReceipt {
                    delivery_id: descendant.delivery_id.clone(),
                    fencing_token: 1,
                },
            )
            .unwrap();
        workflow
            .request_control(
                CollectionRequestedOutcome::Cancelled,
                None,
                "CancelChecks".to_string(),
                2,
                authority,
                None,
            )
            .unwrap();
        append_collection_record_idempotent(&store, 0, "Controlled", &workflow)
            .await
            .unwrap();

        let member_delivery_id = descendant.delivery_id.clone();
        descendant.delivery_id.push_str("-descendant");
        descendant.root_delivery_id = member_delivery_id;
        let context = descendant.collection.as_mut().unwrap();
        context.role = CollectionDeliveryRole::MemberDescendant;
        context.control_epoch = 0;
        let delivery_journal = delivery_journal_id(&descendant);
        let (pending, pending_sequence) = load_delivery_record(&store, descendant.clone())
            .await
            .unwrap();
        append_delivery_record(&store, pending_sequence, &pending)
            .await
            .unwrap();
        sim.inject_concurrency_violations(&delivery_journal, 1);
        let dispatcher = ReactionDispatcher::new(Arc::new(ReactionRegistry::new()));
        dispatcher
            .dispatch_committed_intent(&state, descendant.clone())
            .await
            .unwrap();
        let (deferred, _) = load_delivery_record(&store, descendant.clone())
            .await
            .unwrap();
        assert_eq!(deferred.status, ReactionDeliveryStatus::Pending);

        dispatcher
            .dispatch_committed_intent(&state, descendant.clone())
            .await
            .unwrap();
        let (skipped, _) = load_delivery_record(&store, descendant).await.unwrap();
        assert_eq!(
            skipped.status,
            ReactionDeliveryStatus::Skipped,
            "unexpected terminal error: {:?}",
            skipped.last_error
        );
        assert_eq!(
            skipped.last_error.as_deref(),
            Some("CollectionControlBeforeDescendantCommit")
        );
        let (replayed, _) = load_collection_record(&store, tenant_name, &workflow.workflow_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(replayed.status, CollectionWorkflowStatus::Cancelling);
        assert_eq!(replayed.join_status, CollectionJoinStatus::Pending);
        assert_eq!(replayed.counts.in_flight, 1);
    }
}
