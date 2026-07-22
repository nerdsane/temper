//! Terminal metadata finalization for a completed OS-app runtime generation.

use std::future::Future;

use super::super::*;

pub(in crate::os_apps) async fn finalize_os_app_publication<F, P>(
    state: &PlatformState,
    publication_guard: &mut SpecPublicationGuard,
    tenant: &TenantId,
    schedule_post_cutover_maintenance: P,
    terminal_metadata_write: F,
) -> Result<(), String>
where
    F: Future<Output = Result<(), String>>,
    P: FnOnce(),
{
    state
        .server
        .complete_spec_publication_retry(publication_guard, tenant)?;
    schedule_post_cutover_maintenance();
    // The terminal `installed` row is a derived recovery marker, not part of
    // the runtime-generation fingerprint. Release the exact armed generation
    // before this cancellable, outcome-ambiguous acknowledgement: a retry can
    // safely reconcile either the preceding `publishing` row or the committed
    // terminal row without inheriting an unreconstructible sticky intent.
    terminal_metadata_write.await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ambiguous_terminal_metadata_ack_cannot_strand_a_completed_runtime_generation() {
        let maintenance_scheduled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let state = PlatformState::new(None);
        let tenant = TenantId::new("terminal-metadata-ambiguity");
        let mut publication = state
            .server
            .begin_spec_publication(&tenant)
            .await
            .expect("acquire publication writer");
        let intent = temper_server::ServerState::spec_publication_intent(
            "terminal-metadata-test",
            [("generation", b"complete".as_slice())],
        );
        state
            .server
            .arm_spec_publication(&mut publication, &tenant, &intent)
            .expect("arm runtime generation");

        let maintenance_observer = std::sync::Arc::clone(&maintenance_scheduled);
        let error = finalize_os_app_publication(
            &state,
            &mut publication,
            &tenant,
            move || {
                maintenance_observer.store(true, std::sync::atomic::Ordering::SeqCst);
            },
            async { Err("injected lost terminal metadata acknowledgement".to_string()) },
        )
        .await
        .expect_err("terminal metadata acknowledgement remains visible to the caller");

        assert!(error.contains("lost terminal metadata acknowledgement"));
        assert!(
            !state.server.spec_publication_gated(&tenant),
            "terminal metadata is a derived completion marker and cannot retain runtime-generation debt"
        );
        assert_eq!(state.server.tenant_generation_version(&tenant), 1);
        assert!(
            maintenance_scheduled.load(std::sync::atomic::Ordering::SeqCst),
            "post-cutover maintenance must run before the ambiguous metadata await"
        );
    }

    #[tokio::test]
    async fn cancelled_terminal_metadata_ack_wait_cannot_strand_a_completed_runtime_generation() {
        let maintenance_scheduled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let state = PlatformState::new(None);
        let tenant = TenantId::new("terminal-metadata-cancellation");
        let mut publication = state
            .server
            .begin_spec_publication(&tenant)
            .await
            .expect("acquire publication writer");
        let intent = temper_server::ServerState::spec_publication_intent(
            "terminal-metadata-cancellation-test",
            [("generation", b"complete".as_slice())],
        );
        state
            .server
            .arm_spec_publication(&mut publication, &tenant, &intent)
            .expect("arm runtime generation");
        let task_state = state.clone();
        let task_tenant = tenant.clone();
        let maintenance_observer = std::sync::Arc::clone(&maintenance_scheduled);
        let (write_polled_tx, write_polled_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            finalize_os_app_publication(
                &task_state,
                &mut publication,
                &task_tenant,
                move || {
                    maintenance_observer.store(true, std::sync::atomic::Ordering::SeqCst);
                },
                async move {
                    let _ = write_polled_tx.send(());
                    std::future::pending::<Result<(), String>>().await
                },
            )
            .await
        });
        write_polled_rx
            .await
            .expect("terminal metadata write should begin");
        task.abort();
        assert!(
            task.await
                .expect_err("finalization task should cancel")
                .is_cancelled(),
            "test must cancel during the outcome-ambiguous terminal metadata await"
        );

        assert!(
            !state.server.spec_publication_gated(&tenant),
            "cancellation after a possible metadata commit cannot retain runtime-generation debt"
        );
        assert_eq!(state.server.tenant_generation_version(&tenant), 1);
        assert!(
            maintenance_scheduled.load(std::sync::atomic::Ordering::SeqCst),
            "post-cutover maintenance must be scheduled before cancellation is possible"
        );
    }
}
