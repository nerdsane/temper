/// WakeupSchedulerIntegration — drives the SleepProcess primitive.
///
/// When a Process executes `SleepProcess { sleep_seconds }`, the spec fires
/// a `schedule_wakeup` trigger. This actor receives it, sleeps, then sends
/// `Resume` back to the Process so it wakes up.
use async_trait::async_trait;
use temper_actor_runtime::{Actor, ActorContext, ActorError, ActorHandle, Message};

use crate::common::{decode_params, message_action, session_id_from_namespace};

pub struct WakeupSchedulerIntegration;

#[async_trait]
impl Actor for WakeupSchedulerIntegration {
    fn actor_type(&self) -> &str {
        "WakeupSchedulerIntegration"
    }

    fn initial_state(&self) -> Vec<u8> {
        vec![]
    }

    async fn handle(
        &self,
        ctx: &ActorContext,
        _state: &mut Vec<u8>,
        message: &Message,
    ) -> Result<(), ActorError> {
        if message_action(message) != "schedule_wakeup" {
            return Ok(());
        }

        // Extract sleep_seconds from SpecMessage params.
        let params = decode_params(message);
        let sleep_secs = params["sleep_seconds"].as_u64().unwrap_or(1);

        tracing::info!(sleep_secs, "WakeupScheduler: sleeping");
        tokio::time::sleep(std::time::Duration::from_secs(sleep_secs)).await;

        // Wake up the Process by sending Resume.
        let namespace = ctx.self_handle().namespace.clone();
        let process_id = session_id_from_namespace(&namespace).to_string();
        let process = ActorHandle::new(namespace, "Process".to_string());
        ctx.tell(
            &process,
            temper_actor_runtime::spec_actor::SpecMessage::new("Resume"),
        )
        .await?;
        tracing::info!(process_id, "WakeupScheduler: sent Resume");
        Ok(())
    }
}
