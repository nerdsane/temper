use super::*;

impl SpecDrivenActor {
    pub(super) async fn execute_commands(
        &self,
        state: &SpecActorState,
        execution: EffectExecution,
        cascade_depth: u32,
        ctx: &ActorContext,
    ) -> Result<(), ActorError> {
        let EffectExecution {
            emitted_events,
            custom_effects,
            scheduled_actions,
            schedule_at_requests,
            spawn_requests,
        } = execution;

        for event in emitted_events {
            self.route_command(state, &event, cascade_depth, ctx)
                .await?;
        }
        for trigger in custom_effects {
            self.route_command(state, &trigger, cascade_depth, ctx)
                .await?;
        }
        for scheduled in scheduled_actions {
            let delay_seconds = i64::try_from(scheduled.delay_seconds).map_err(|_| {
                ActorError::HandlerFailed(format!(
                    "scheduled action {} exceeds duration budget",
                    scheduled.action
                ))
            })?;
            ctx.tell_after(
                ctx.self_handle(),
                SpecMessage::new(scheduled.action),
                chrono::Duration::seconds(delay_seconds),
            )
            .await?;
        }
        for request in schedule_at_requests {
            let deliver_at = schedule_at_timestamp(state, &request.field)?;
            ctx.tell_at(
                ctx.self_handle(),
                SpecMessage::new(request.action),
                deliver_at,
            )
            .await?;
        }

        if spawn_requests.len() > 8 {
            return Err(ActorError::HandlerFailed(
                "spawn effect budget exceeded (8 per transition)".to_string(),
            ));
        }
        for spawn in spawn_requests {
            let namespace = child_namespace(&ctx.self_handle().namespace, &spawn.entity_id);
            let target = ActorHandle::new(namespace, spawn.entity_type);
            let fields = serde_json::Value::Object(spawn.copied_field_values);
            let initial_message = spawn.initial_action.map(|action| {
                let message = SpecMessage::with_params(action, fields.clone());
                BufferedTell {
                    to: target.clone(),
                    message_type: "SpecMessage".to_string(),
                    payload: prost::Message::encode_to_vec(&message),
                    correlation_id: None,
                    deliver_at: None,
                }
            });
            ctx.buffer_spawn(target, fields, initial_message).await?;
        }

        Ok(())
    }

    async fn route_command(
        &self,
        state: &SpecActorState,
        command: &str,
        cascade_depth: u32,
        ctx: &ActorContext,
    ) -> Result<(), ActorError> {
        let rules: Vec<ReactionRule> = self
            .reactions
            .lookup(&self.name, command, &state.status)
            .into_iter()
            .cloned()
            .collect();
        if rules.is_empty() {
            tracing::warn!(
                actor = %self.name,
                command,
                "no reaction rule for effect command"
            );
            return Ok(());
        }
        self.route_rules(state, command, rules, cascade_depth, ctx)
            .await
    }

    async fn route_rules(
        &self,
        state: &SpecActorState,
        command: &str,
        rules: Vec<ReactionRule>,
        cascade_depth: u32,
        ctx: &ActorContext,
    ) -> Result<(), ActorError> {
        if cascade_depth >= temper_runtime::reaction::MAX_REACTION_DEPTH {
            tracing::warn!(
                actor = %self.name,
                command,
                cascade_depth,
                "reaction cascade depth budget reached"
            );
            return Ok(());
        }
        for rule in rules {
            let Some(target) = self.resolve_reaction_target(state, &rule, ctx).await? else {
                tracing::warn!(
                    actor = %self.name,
                    command,
                    reaction = %rule.name,
                    "reaction target could not be resolved; source transition remains committed"
                );
                continue;
            };
            tracing::info!(
                actor = %self.name,
                command,
                reaction = %rule.name,
                target = %target,
                target_action = %rule.then.action,
                "routing effect command"
            );
            ctx.tell(
                &target,
                SpecMessage::routed(rule.then.action, cascade_depth + 1),
            )
            .await?;
        }
        Ok(())
    }

    async fn resolve_reaction_target(
        &self,
        state: &SpecActorState,
        rule: &ReactionRule,
        ctx: &ActorContext,
    ) -> Result<Option<ActorHandle>, ActorError> {
        let namespace = match &rule.resolve_target {
            TargetResolver::SameId => ctx.self_handle().namespace.clone(),
            TargetResolver::Field { field } => {
                let Some(entity_id) = optional_string_field(state, field) else {
                    return Ok(None);
                };
                entity_namespace(&ctx.self_handle().namespace, entity_id)
            }
            TargetResolver::Static { entity_id } => {
                entity_namespace(&ctx.self_handle().namespace, entity_id)
            }
            TargetResolver::CreateIfMissing { id_field } => {
                let source_id = ctx
                    .self_handle()
                    .namespace
                    .rsplit('/')
                    .next()
                    .unwrap_or(&ctx.self_handle().namespace);
                let derived_id = format!("{source_id}-derived");
                let entity_id = optional_string_field(state, id_field).unwrap_or(&derived_id);
                let namespace = child_namespace(&ctx.self_handle().namespace, entity_id);
                let target = ActorHandle::new(namespace.clone(), rule.then.entity_type.clone());
                ctx.buffer_spawn(target, serde_json::json!({}), None)
                    .await?;
                namespace
            }
        };
        Ok(Some(ActorHandle::new(
            namespace,
            rule.then.entity_type.clone(),
        )))
    }
}

fn optional_string_field<'a>(state: &'a SpecActorState, field: &str) -> Option<&'a str> {
    state
        .fields
        .get(field)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
}

fn required_string_field<'a>(
    state: &'a SpecActorState,
    field: &str,
) -> Result<&'a str, ActorError> {
    optional_string_field(state, field).ok_or_else(|| {
        ActorError::HandlerFailed(format!(
            "reaction target field {field:?} is missing or not a non-empty string"
        ))
    })
}

fn child_namespace(parent_namespace: &str, child_id: &str) -> String {
    match parent_namespace.rsplit_once('/') {
        Some((prefix, _)) => format!("{prefix}/{child_id}"),
        None => child_id.to_string(),
    }
}

fn entity_namespace(source_namespace: &str, entity_id: &str) -> String {
    if entity_id.contains('/') {
        entity_id.to_string()
    } else {
        child_namespace(source_namespace, entity_id)
    }
}

fn schedule_at_timestamp(
    state: &SpecActorState,
    field: &str,
) -> Result<chrono::DateTime<chrono::Utc>, ActorError> {
    let value = required_string_field(state, field)?;
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.with_timezone(&chrono::Utc))
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S")
                .map(|timestamp| timestamp.and_utc())
        })
        .map_err(|error| {
            ActorError::HandlerFailed(format!(
                "schedule_at field {field:?} is not a valid timestamp: {error}"
            ))
        })
}
