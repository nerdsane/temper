//! Retried append entry points.

use super::*;

impl TursoEventStore {
    #[instrument(skip_all, fields(persistence_id, otel.name = "turso.append"))]
    pub(super) async fn append_impl(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
    ) -> Result<u64, PersistenceError> {
        if events.is_empty() {
            return Ok(expected_sequence);
        }

        // Retry transient Hrana BLOCKED / stream errors with backoff (ADR-0056).
        // Each attempt is a complete append unit. Single-event appends use an
        // atomic conditional insert; multi-event appends open a transaction.
        // Event-store's UNIQUE (entity_type, entity_id, sequence_nr) makes
        // retries safe — if a prior attempt partially committed before erroring,
        // the retry's pre-check detects it as ConcurrencyViolation
        // (non-transient, propagates to caller via normal event-store contract).
        let attempt_timeout = append_attempt_timeout();
        let total_attempts = append_max_attempts();
        let mut last_err: Option<PersistenceError> = None;
        let bypass_write_gate = events.len() == 1;
        for attempt in 0..total_attempts {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(retry_delay_ms(attempt - 1))).await;
            }
            let _high_priority_marker = if bypass_write_gate {
                Some(self.mark_high_priority_write("turso.append"))
            } else {
                None
            };
            let _write_permit = if bypass_write_gate {
                None
            } else {
                Some(
                    self.acquire_write_permit("turso.append", WritePriority::High)
                        .await?,
                )
            };
            let attempt_result = tokio::time::timeout(
                attempt_timeout,
                self.append_inner(persistence_id, expected_sequence, events),
            )
            .await
            .unwrap_or_else(|_| {
                warn!(
                    persistence_id,
                    attempt,
                    timeout_ms = attempt_timeout.as_millis() as u64,
                    "turso.append attempt timed out"
                );
                Err(PersistenceError::Storage(format!(
                    "turso.append timed out after {}ms",
                    attempt_timeout.as_millis()
                )))
            });

            match attempt_result {
                Ok(seq) => {
                    if attempt > 0 {
                        record_turso_write_retry("turso.append", attempt as u64, "succeeded");
                    }
                    return Ok(seq);
                }
                Err(err) => {
                    let transient = match &err {
                        PersistenceError::Storage(msg) => is_transient_write_error(msg),
                        _ => false,
                    };
                    if !transient {
                        return Err(err);
                    }
                    last_err = Some(err);
                }
            }
        }
        record_turso_write_retry("turso.append", total_attempts as u64, "exhausted");
        Err(last_err.expect("retry loop captured at least one error"))
    }

    #[instrument(skip_all, fields(otel.name = "turso.append_batch"))]
    pub(super) async fn append_batch_impl(
        &self,
        appends: &[PersistenceAppend],
    ) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
        if appends.is_empty() {
            return Ok(Vec::new());
        }
        if let [append] = appends {
            let sequence_nr = self
                .append(
                    &append.persistence_id,
                    append.expected_sequence,
                    &append.events,
                )
                .await?;
            return Ok(vec![PersistenceAppendResult {
                persistence_id: append.persistence_id.clone(),
                sequence_nr,
            }]);
        }

        let attempt_timeout = append_attempt_timeout();
        let total_attempts = append_max_attempts();
        let mut last_err: Option<PersistenceError> = None;
        for attempt in 0..total_attempts {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(retry_delay_ms(attempt - 1))).await;
            }
            let _write_permit = self
                .acquire_write_permit("turso.append_batch", WritePriority::High)
                .await?;
            let attempt_result =
                tokio::time::timeout(attempt_timeout, self.append_batch_inner(appends))
                    .await
                    .unwrap_or_else(|_| {
                        warn!(
                            attempt,
                            timeout_ms = attempt_timeout.as_millis() as u64,
                            "turso.append_batch attempt timed out"
                        );
                        Err(PersistenceError::Storage(format!(
                            "turso.append_batch timed out after {}ms",
                            attempt_timeout.as_millis()
                        )))
                    });

            match attempt_result {
                Ok(result) => {
                    if attempt > 0 {
                        record_turso_write_retry("turso.append_batch", attempt as u64, "succeeded");
                    }
                    return Ok(result);
                }
                Err(err) => {
                    let transient = matches!(&err, PersistenceError::Storage(msg) if is_transient_write_error(msg));
                    if !transient {
                        return Err(err);
                    }
                    last_err = Some(err);
                }
            }
        }
        record_turso_write_retry("turso.append_batch", total_attempts as u64, "exhausted");
        Err(last_err.expect("retry loop captured at least one error"))
    }
}
