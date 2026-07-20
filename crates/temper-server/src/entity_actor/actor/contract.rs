//! Temper runtime actor contract for entity actors.

use super::*;

impl Actor for EntityActor {
    type Msg = EntityMsg;
    type State = EntityState;

    async fn pre_start(&self, _ctx: &mut ActorContext<Self>) -> Result<Self::State, ActorError> {
        self.pre_start_state().await
    }

    async fn handle(
        &self,
        msg: Self::Msg,
        state: &mut Self::State,
        ctx: &mut ActorContext<Self>,
    ) -> Result<(), ActorError> {
        match msg {
            EntityMsg::Action {
                name,
                params,
                cross_entity_booleans,
                idempotency_key,
                state_timeout_precondition,
            } => {
                self.handle_action(
                    EntityActionRequest {
                        name,
                        params,
                        cross_entity_booleans,
                        idempotency_key,
                        state_timeout_precondition,
                    },
                    state,
                    ctx,
                )
                .await?;
            }
            EntityMsg::GetState => {
                ctx.reply(EntityResponse {
                    success: true,
                    state: state.clone(),
                    error: None,
                    custom_effects: vec![],
                    scheduled_actions: vec![],
                    spawn_requests: vec![],
                    spec_governed: true,
                });
            }
            EntityMsg::GetField { field } => {
                let value = state
                    .fields
                    .get(&field)
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                ctx.reply(value);
            }
            EntityMsg::UpdateFields { fields, replace } => {
                if replace {
                    // PUT: replace all fields (preserve Id and Status)
                    let id = state.entity_id.clone();
                    let status = state.status.clone();
                    state.fields = fields;
                    if let Some(obj) = state.fields.as_object_mut() {
                        obj.insert("Id".to_string(), serde_json::Value::String(id));
                        obj.insert("Status".to_string(), serde_json::Value::String(status));
                    }
                } else {
                    // PATCH: merge fields into existing
                    if let (Some(existing), Some(updates)) =
                        (state.fields.as_object_mut(), fields.as_object())
                    {
                        for (k, v) in updates {
                            existing.insert(k.clone(), v.clone());
                        }
                    }
                }
                ctx.reply(EntityResponse {
                    success: true,
                    state: state.clone(),
                    error: None,
                    custom_effects: vec![],
                    scheduled_actions: vec![],
                    spawn_requests: vec![],
                    spec_governed: true,
                });
            }
            EntityMsg::Delete => {
                let table = self.table.read().expect("table lock poisoned").clone();
                let deleted = EntityEvent {
                    action: "Deleted".to_string(),
                    from_status: state.status.clone(),
                    to_status: "Deleted".to_string(),
                    timestamp: sim_now(),
                    params: serde_json::json!({}),
                    idempotency_key: None,
                };
                let mut persisted_timeout_clock = None;

                if let (Some(store), Some(backend)) =
                    (self.event_journal.as_ref(), self.event_backend)
                {
                    match self
                        .persist_event(
                            store,
                            backend,
                            &self.persistence_id(),
                            &table,
                            state,
                            &deleted,
                        )
                        .await
                    {
                        Ok((_, clock)) => persisted_timeout_clock = Some(clock),
                        Err(e) => {
                            ctx.reply(EntityResponse {
                                success: false,
                                state: state.clone(),
                                error: Some(format!("persistence failed: {e}")),
                                custom_effects: vec![],
                                scheduled_actions: vec![],
                                spawn_requests: vec![],
                                spec_governed: true,
                            });
                            return Ok(());
                        }
                    }
                }

                state.status = deleted.to_status.clone();
                if let Some(obj) = state.fields.as_object_mut() {
                    obj.insert(
                        "Status".to_string(),
                        serde_json::Value::String(state.status.clone()),
                    );
                }
                if let Some(clock) = persisted_timeout_clock {
                    apply_state_timeout_clock(state, clock);
                } else {
                    Self::update_state_timeout_clock(&table, state, &deleted);
                }
                state.push_event_bounded(deleted);

                ctx.reply(EntityResponse {
                    success: true,
                    state: state.clone(),
                    error: None,
                    custom_effects: vec![],
                    scheduled_actions: vec![],
                    spawn_requests: vec![],
                    spec_governed: true,
                });
            }
        }
        Ok(())
    }

    async fn post_stop(&self, state: Self::State, _ctx: &mut ActorContext<Self>) {
        tracing::info!(
            entity = %state.entity_id,
            status = %state.status,
            events_total = state.total_event_count,
            events_recent = state.events.len(),
            "entity actor stopped"
        );
    }
}
