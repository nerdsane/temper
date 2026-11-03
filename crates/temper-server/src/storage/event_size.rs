//! How large one recorded change may be: the warning size and the limit every
//! append through [`BoxedEventStore`] is held to (`TEMPER_WARN_EVENT_BYTES`,
//! `TEMPER_MAX_EVENT_BYTES`, read by `temper serve`). Off unless set.

use temper_runtime::persistence::{PersistenceAppend, PersistenceEnvelope, PersistenceError};

use super::BoxedEventStore;

/// How large one event (one recorded change, its payload as JSON) may be.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EventSizeLimits {
    /// Above this many bytes an event is written and a warning is logged.
    pub warn_bytes: Option<usize>,
    /// Above this many bytes an append is refused and nothing is written.
    pub max_bytes: Option<usize>,
}

impl BoxedEventStore {
    /// Warn about, or refuse, any append carrying an event whose payload, as
    /// JSON, is larger than the limits. The default warns and refuses nothing.
    pub fn with_event_size_limits(mut self, limits: EventSizeLimits) -> Self {
        debug_assert!(
            limits.warn_bytes != Some(0) && limits.max_bytes != Some(0),
            "a zero limit would catch every event"
        );
        self.limits = limits;
        self
    }

    /// The size limits every append is held to.
    pub(crate) fn event_size_limits(&self) -> EventSizeLimits {
        self.limits
    }

    /// Warn about, or refuse, `events` by the limits; every append checks.
    pub(crate) fn check_event_sizes(
        &self,
        persistence_id: &str,
        events: &[PersistenceEnvelope],
    ) -> Result<(), PersistenceError> {
        self.check_appended(events.iter().map(|event| (persistence_id, event)))
    }

    /// Hold the events of one append (each with its journal) to the limits. Every
    /// one is held to the limit before any is warned about, so a refused append
    /// logs its refusal once and warns about nothing it did not write.
    pub(super) fn check_appended<'a>(
        &self,
        events: impl Iterator<Item = (&'a str, &'a PersistenceEnvelope)>,
    ) -> Result<(), PersistenceError> {
        let EventSizeLimits {
            warn_bytes,
            max_bytes,
        } = self.limits;
        if warn_bytes.is_none() && max_bytes.is_none() {
            return Ok(());
        }
        let mut sized = Vec::new();
        for (persistence_id, event) in events {
            sized.push((persistence_id, event, json_len(&event.payload)?));
        }
        for &(persistence_id, event, bytes) in &sized {
            self.refuse_if_over(persistence_id, &event.event_type, bytes)?;
        }
        for &(persistence_id, event, bytes) in &sized {
            self.warn_if_over(persistence_id, &event.event_type, bytes);
        }
        Ok(())
    }

    /// Refuse one event's payload over the limit, logging the refusal once, and,
    /// when `warn`, log one over the warning size.
    pub(crate) fn check_event_size(
        &self,
        persistence_id: &str,
        event_type: &str,
        payload: &serde_json::Value,
        warn: bool,
    ) -> Result<(), PersistenceError> {
        let EventSizeLimits {
            warn_bytes,
            max_bytes,
        } = self.limits;
        if max_bytes.is_none() && (!warn || warn_bytes.is_none()) {
            return Ok(());
        }
        let bytes = json_len(payload)?;
        self.refuse_if_over(persistence_id, event_type, bytes)?;
        if warn {
            self.warn_if_over(persistence_id, event_type, bytes);
        }
        Ok(())
    }

    fn refuse_if_over(
        &self,
        persistence_id: &str,
        event_type: &str,
        bytes: usize,
    ) -> Result<(), PersistenceError> {
        let Some(limit) = self.limits.max_bytes else {
            return Ok(());
        };
        if bytes <= limit {
            return Ok(());
        }
        tracing::warn!(
            persistence_id,
            event_type,
            bytes,
            max_bytes = limit,
            "event refused for its size: one change was larger than this server's limit; keep long lists as their own records and pictures as files"
        );
        Err(PersistenceError::EventTooLarge {
            event_type: event_type.to_string(),
            persistence_id: persistence_id.to_string(),
            bytes,
            limit,
        })
    }

    fn warn_if_over(&self, persistence_id: &str, event_type: &str, bytes: usize) {
        if let Some(warn_at) = self.limits.warn_bytes
            && bytes > warn_at
        {
            tracing::warn!(
                persistence_id,
                event_type,
                bytes,
                warn_bytes = warn_at,
                max_bytes = self.limits.max_bytes,
                "large event: one change recorded more than the warning size; keep long lists as their own records and pictures as files"
            );
        }
    }

    /// Refuse the appends' events over the limit without warning: for a write that
    /// must know before it stores anything else (overflow blobs), ahead of the
    /// append itself, which warns.
    pub(crate) fn refuse_oversized(
        &self,
        appends: &[PersistenceAppend],
    ) -> Result<(), PersistenceError> {
        if self.limits.max_bytes.is_none() {
            return Ok(());
        }
        for append in appends {
            for event in &append.events {
                self.check_event_size(
                    &append.persistence_id,
                    &event.event_type,
                    &event.payload,
                    false,
                )?;
            }
        }
        Ok(())
    }
}

/// The length of `value` serialized as JSON, counted without building the text.
fn json_len(value: &serde_json::Value) -> Result<usize, PersistenceError> {
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 += buf.len();
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    serde_json::to_writer(&mut count, value)
        .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
    Ok(count.0)
}
