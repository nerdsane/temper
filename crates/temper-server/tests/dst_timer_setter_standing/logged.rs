//! The withheld fires the server logs, captured from its tracing.

use std::sync::{Arc, Mutex, OnceLock};

use tracing_subscriber::prelude::*;

/// The withheld fires logged: the record and the setter each names.
#[derive(Clone, Default)]
pub(super) struct Withheld(Arc<Mutex<Vec<(String, String)>>>);

impl Withheld {
    /// Capture across the test binary, once. With a scoped subscriber, another
    /// test can first reach a callsite with no subscriber and cache it as
    /// disabled, losing this test's warnings too. The global default keeps
    /// callsite registration consistent; assertions select their own records.
    pub(super) fn install() -> Self {
        static LOGS: OnceLock<Withheld> = OnceLock::new();
        LOGS.get_or_init(|| {
            let logs = Self::default();
            tracing::subscriber::set_global_default(
                tracing_subscriber::registry().with(logs.clone()),
            )
            .expect("the standing tests install one shared subscriber");
            logs
        })
        .clone()
    }

    pub(super) fn of(&self, id: &str) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|(record, _)| record == id)
            .map(|(_, setter)| setter.clone())
            .collect()
    }

    pub(super) fn clear(&self) {
        self.0.lock().unwrap().clear();
    }
}

/// The fields of a log line that name a withheld fire.
#[derive(Default)]
struct LineFields {
    message: String,
    entity_id: String,
    setter: String,
}

impl tracing::field::Visit for LineFields {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.set(field.name(), value.to_string());
    }
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.set(
            field.name(),
            format!("{value:?}").trim_matches('"').to_string(),
        );
    }
}

impl LineFields {
    fn set(&mut self, name: &str, value: String) {
        match name {
            "message" => self.message = value,
            "entity_id" => self.entity_id = value,
            "setter" => self.setter = value,
            _ => {}
        }
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Withheld {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = LineFields::default();
        event.record(&mut fields);
        if fields.message.starts_with("timer fire withheld") {
            self.0
                .lock()
                .unwrap()
                .push((fields.entity_id, fields.setter));
        }
    }
}
