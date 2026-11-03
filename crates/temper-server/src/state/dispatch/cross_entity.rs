use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::DispatchExtOptions;
use super::owed_writes::{Attempt, OwedKind, OwedTry, OwedWrite};
use crate::idempotency::{IdempotencyKey, derived_idempotency_key};
use crate::request_context::AgentContext;
use crate::state::CreateError;
use temper_jit::table::{Related, RelatedMap, TransitionTable};
use temper_runtime::tenant::TenantId;
use tracing::Instrument;

impl crate::state::ServerState {
    /// Resolve the related entities a move of `action` reads: its guards, and
    /// the type's invariants, which every move is checked against.
    ///
    /// For each `Type[id_field].status` they read, reads the
    /// id (or list of ids) from stored fields or declared defaults and resolves each
    /// related entity's status. An unset reference resolves to no statuses;
    /// an entity that is not found resolves to `None`. Every id is resolved:
    /// the number of lookups is the number of ids the entity's own fields
    /// hold, so a guard over all of a list's entities sees all of them.
    pub(super) async fn resolve_related_statuses(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        action: &str,
    ) -> RelatedMap {
        let table = {
            let registry = self.registry.read().unwrap(); // ci-ok: infallible lock
            let Some(spec) = registry.get_spec(tenant, entity_type) else {
                return RelatedMap::new();
            };
            spec.table()
        };
        let refs = table.related_refs(action);
        if refs.is_empty() {
            return RelatedMap::new();
        }

        // Get current entity fields to resolve target entity IDs
        let current_fields = match self
            .get_tenant_entity_state(tenant, entity_type, entity_id)
            .await
        {
            Ok(resp) => resp.state.fields,
            Err(_) => return RelatedMap::new(),
        };
        self.related_statuses_of(tenant, &table, refs, &current_fields)
            .await
    }

    /// The statuses of the records `fields` refers to through each of
    /// `refs`, `(related type, id field)`, under `table`'s declared defaults,
    /// as [`Self::resolve_related_statuses`] resolves them.
    pub(in crate::state) async fn related_statuses_of(
        &self,
        tenant: &TenantId,
        table: &TransitionTable,
        refs: Vec<(String, String)>,
        fields: &serde_json::Value,
    ) -> RelatedMap {
        self.resolve_guard_refs(tenant, table, refs, fields, None)
            .await
    }

    /// The related references used by the move's guards and invariants.
    pub(super) fn guard_related_refs(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        action: &str,
    ) -> Vec<(String, String)> {
        let registry = self.registry.read().unwrap(); // ci-ok: infallible lock
        registry
            .get_spec(tenant, entity_type)
            .map(|spec| spec.table().related_refs(action))
            .unwrap_or_default()
    }

    /// Resolve related statuses, reading the spawning parent's committed result.
    pub(super) async fn resolve_guard_refs(
        &self,
        tenant: &TenantId,
        table: &TransitionTable,
        refs: Vec<(String, String)>,
        fields: &serde_json::Value,
        committed: Option<&crate::entity_actor::EntityState>,
    ) -> RelatedMap {
        let mut result = RelatedMap::new();
        for (target_type, id_field) in refs {
            let ids = crate::entity_actor::related::reference_ids(
                fields,
                &table.initial_values,
                &id_field,
            );
            let mut statuses = Vec::with_capacity(ids.len());
            for id in ids {
                let status = match committed {
                    Some(record) if record.entity_type == target_type && record.entity_id == id => {
                        Some(record.status.clone())
                    }
                    _ => self.resolve_entity_status(tenant, &target_type, id).await,
                };
                statuses.push(status);
            }
            result.insert((target_type, id_field), Related::Statuses(statuses));
        }
        result
    }

    /// Dispatch entity spawn requests post-transition.
    ///
    /// This is a **sync** method (like `arm_scheduled_actions`) so that
    /// `tokio::spawn` inside it does not cause async recursion.
    /// Creates child entities and optionally dispatches initial actions.
    ///
    /// Every request is dispatched. Each one comes from a `spawn(...)` effect
    /// written in the spec, so their number is fixed by the spec text.
    pub(super) fn dispatch_spawn_requests(
        &self,
        tenant: &TenantId,
        parent_type: &str,
        parent_id: &str,
        spawn_requests: &[crate::entity_actor::effects::SpawnRequest],
        action_params: &serde_json::Value,
        agent_ctx: &AgentContext,
    ) {
        // Match the preview: independent children run concurrently; each child's
        // initializers run in effect order, including after a store failure.
        let mut by_child = std::collections::BTreeMap::<_, Vec<_>>::new();
        for req in spawn_requests {
            by_child
                .entry((req.entity_type.clone(), req.entity_id.clone()))
                .or_default()
                .push(req.clone());
        }
        for ((child_type, child_id), requests) in by_child {
            let state = self.clone();
            let t = tenant.clone();
            let parent_t = parent_type.to_string();
            let parent_i = parent_id.to_string();

            let parent_params = action_params.clone();
            let mut agent = agent_ctx.clone();
            agent.event_origin = Some(crate::entity_actor::EventOrigin::reaction_as(agent_ctx));
            let workflow_root_entity_type = agent
                .workflow_root_entity_type
                .clone()
                .unwrap_or_else(|| parent_t.clone());
            let workflow_root_entity_id = agent
                .workflow_root_entity_id
                .clone()
                .unwrap_or_else(|| parent_i.clone());
            let workflow_run_id = agent
                .workflow_run_id
                .clone()
                .unwrap_or_else(|| format!("{parent_t}:{parent_i}"));
            let span = crate::trace_context::runtime_work_span!(
                crate::trace_context::cause(),
                "dispatch.background_spawn_entity",
                workflow.root_entity_type = %workflow_root_entity_type,
                workflow.root_entity_id = %workflow_root_entity_id,
                workflow.run_id = %workflow_run_id,
                parent_type = %parent_t,
                parent_id = %parent_i,
                child_type = %child_type,
                child_id = %child_id,
            );

            tokio::spawn(
                async move {
                    // determinism-ok: spawn dispatch is a background side-effect
                    let mut children = Vec::new();
                    let repeated = requests.len() > 1;
                    for (index, req) in requests.iter().enumerate() {
                        // Validation happens in spawn_child, which preserves typed
                        // store errors so owed writes can be retried.
                        let plan = match state
                            .spawn_plan(&t, &parent_t, &parent_i, req, &parent_params, true)
                            .await
                        {
                            Ok(plan) => plan,
                            Err(refusal) => {
                                state
                                    .refuse_spawn(
                                        &t,
                                        &parent_t,
                                        &parent_i,
                                        req,
                                        &refusal.action,
                                        &refusal.error,
                                    )
                                    .await;
                                continue;
                            }
                        };
                        let super::spawn_plan::SpawnPlan {
                            table,
                            initial_fields,
                            initializer,
                        } = plan;
                        let mut agent = agent.clone();
                        agent.idempotency_key = agent.idempotency_key.as_ref().map(|key| {
                            let identity = if repeated {
                                format!("{child_type}:{child_id}:{index}")
                            } else {
                                format!("{child_type}:{child_id}")
                            };
                            IdempotencyKey::FollowUp(derived_idempotency_key(
                                key.as_str(),
                                "spawn",
                                &identity,
                            ))
                        });
                        // The child is an entity this chain made, and its initial
                        // action is the chain's next step. One that would repeat a
                        // step the chain is already in is a loop (`InlineChain`),
                        // refused before the child is made: a spawn loop stops at
                        // its first repeat like a reaction loop.
                        let mut agent =
                            agent.having_made([(child_type.as_str(), child_id.as_str())]);
                        if let Some((action, _)) = &initializer {
                            let step = agent.inline_chain.step(&child_type, &child_id, action);
                            match agent.inline_chain.then(step) {
                                Ok(chain) => agent.inline_chain = chain,
                                Err(repeat) => {
                                    state
                                        .refuse_spawn(
                                            &t,
                                            &parent_t,
                                            &parent_i,
                                            req,
                                            action,
                                            &format!(
                                                "spawned {child_type} not made: its initial action \
                                         would repeat a step its chain is already in \
                                         ({repeat}), so the chain is a loop"
                                            ),
                                        )
                                        .await;
                                    continue;
                                }
                            }
                        }
                        let child = ChildSpawn {
                            request: req.clone(),
                            tenant: t.clone(),
                            parent_type: parent_t.clone(),
                            parent_id: parent_i.clone(),
                            child_type: child_type.clone(),
                            child_id: child_id.clone(),
                            table,
                            initial_fields,
                            initializer,
                            agent,
                        };
                        children.push(child);
                    }
                    let children = Arc::new(ChildSpawns {
                        children,
                        next: AtomicUsize::new(0),
                    });
                    if let Attempt::StoreFailed(error) = state.spawn_children(&children).await {
                        state.keep_child_spawns(children, &error);
                    }
                }
                .instrument(span),
            );
        }
    }
}

/// One child's ordered initializers, with the first one not yet answered.
/// Keep final refusals as well as successes behind this cursor: re-evaluating
/// a refused initializer after a later one lands could make it succeed.
struct ChildSpawns {
    children: Vec<ChildSpawn>,
    next: AtomicUsize,
}

/// A child a committed move spawns: created, then sent its initial action.
struct ChildSpawn {
    request: crate::entity_actor::effects::SpawnRequest,
    tenant: TenantId,
    parent_type: String,
    parent_id: String,
    child_type: String,
    child_id: String,
    /// The child type's transition table, which its initial action's
    /// parameters are checked against.
    table: Arc<TransitionTable>,
    initial_fields: serde_json::Value,
    initializer: Option<(String, serde_json::Value)>,
    /// The spawning move's identity, under the child's own key.
    agent: AgentContext,
}

impl ChildSpawn {
    fn parent(&self) -> super::WasmEntityRef<'_> {
        super::WasmEntityRef {
            tenant: &self.tenant,
            entity_type: &self.parent_type,
            entity_id: &self.parent_id,
        }
    }
}

impl crate::state::ServerState {
    /// Check `child`'s initial action, create the child, then send the
    /// action. A read of the child, a creation or an initial action the store
    /// did not take is the store's failure; any other failure is final, and a
    /// child whose initial action failed stays in its first state. Creating a
    /// child that exists already reads it, so a try after a creation that
    /// landed goes on to the initial action.
    async fn spawn_child(&self, child: &ChildSpawn) -> Attempt {
        let (tenant, child_type, child_id) = (&child.tenant, &child.child_type, &child.child_id);
        if let Err(attempt) = self.check_child_initializer(child).await {
            return attempt;
        }
        // Its first state's timer keeps whether its creator could read it.
        let creator = match self
            .created_with_read_when_set(
                tenant,
                child_type,
                child_id,
                &child.initial_fields,
                &child.agent,
            )
            .await
        {
            Ok(creator) => creator,
            Err(error) => return Attempt::StoreFailed(error),
        };
        let created = match self
            .get_or_create_tenant_entity_by(
                tenant,
                child_type,
                child_id,
                child.initial_fields.clone(),
                &creator,
            )
            .await
        {
            Ok(created) => created,
            Err(error) => {
                tracing::error!(%child_type, %child_id, %error, "failed to spawn child entity");
                return match error {
                    CreateError::StoreUnavailable(error) => Attempt::StoreFailed(error),
                    CreateError::Failed(error) => {
                        self.refuse_spawn(
                            tenant,
                            &child.parent_type,
                            &child.parent_id,
                            &child.request,
                            "spawn",
                            &error,
                        )
                        .await;
                        Attempt::Failed(error)
                    }
                };
            }
        };
        tracing::info!(parent_type = %child.parent_type, parent_id = %child.parent_id,
            %child_type, %child_id, "spawned child entity");
        let Some((action, params)) = &child.initializer else {
            // A child with no initial action stays in its first state: arm
            // that state's timer here.
            self.arm_state_timeouts_on_creation(tenant, child_type, child_id, &created, &creator)
                .await;
            return Attempt::Landed;
        };
        let options = DispatchExtOptions {
            agent_ctx: &child.agent,
            await_integration: false,
            await_reactions: true,
        };
        let reply = self
            .dispatch_tenant_action_ext_typed(
                tenant,
                child_type,
                child_id,
                action,
                params.clone(),
                options,
            )
            .await;
        let attempt = Attempt::of(&reply);
        if let Attempt::Failed(error) = &attempt {
            // The child stays in its first state: arm that state's timer, as
            // for no initializer.
            self.arm_state_timeouts_on_creation(tenant, child_type, child_id, &created, &creator)
                .await;
            self.record_generated_callback_refusal(child.parent(), action, error);
        }
        attempt
    }

    /// Check `child`'s initial action against the child it would be sent
    /// to. Existing actors validate against their hydrated blob values and
    /// execution-time state; only an absent child needs this check before
    /// creation can persist anything. A read of the child the store failed
    /// is tried again with the spawn; a refused check is final.
    async fn check_child_initializer(&self, child: &ChildSpawn) -> Result<(), Attempt> {
        let Some((action, params)) = &child.initializer else {
            return Ok(());
        };
        let (tenant, child_type, child_id) = (&child.tenant, &child.child_type, &child.child_id);
        let refused = match self
            .load_authz_resource_snapshot(tenant, child_type, child_id)
            .await
        {
            Ok(snapshot) if snapshot.exists => return Ok(()),
            Ok(_) => {
                let table = &child.table;
                let prestate = crate::entity_actor::EntityActor::build_initial_state(
                    child_type,
                    child_id,
                    table,
                    &child.initial_fields,
                );
                match table.validate_action_params(
                    action,
                    params,
                    &prestate.fields,
                    &prestate.counters,
                    &prestate.booleans,
                ) {
                    Ok(()) => return Ok(()),
                    Err(error) => String::from(error),
                }
            }
            Err(error) => match Attempt::of_error(&error) {
                Attempt::Failed(refused) => refused,
                attempt => return Err(attempt),
            },
        };
        self.refuse_spawn(
            tenant,
            &child.parent_type,
            &child.parent_id,
            &child.request,
            action,
            &refused,
        )
        .await;
        Err(Attempt::Failed(refused))
    }

    /// Run the initializers in order; a store failure holds the rest back.
    async fn spawn_children(&self, spawns: &ChildSpawns) -> Attempt {
        for (index, child) in spawns
            .children
            .iter()
            .enumerate()
            .skip(spawns.next.load(Ordering::Relaxed))
        {
            if let failure @ Attempt::StoreFailed(_) = self.spawn_child(child).await {
                return failure;
            }
            spawns.next.store(index + 1, Ordering::Relaxed);
        }
        Attempt::Landed
    }

    /// Resume at the first unanswered initializer, under its stable key.
    fn keep_child_spawns(&self, children: Arc<ChildSpawns>, error: &str) {
        let Some(child) = children.children.first() else {
            return;
        };
        let owed = OwedWrite {
            tenant: child.tenant.clone(),
            entity_type: child.child_type.clone(),
            entity_id: child.child_id.clone(),
            kind: OwedKind::Child,
            key: child.agent.idempotency_key.clone(),
        };
        self.keep_owed_write(owed, error, move |state| {
            spawn_children_again(state, Arc::clone(&children))
        });
    }
}

fn spawn_children_again(state: crate::state::ServerState, children: Arc<ChildSpawns>) -> OwedTry {
    Box::pin(async move { state.spawn_children(&children).await })
}

#[cfg(test)]
#[path = "cross_entity_test.rs"]
mod required_ref_tests;
