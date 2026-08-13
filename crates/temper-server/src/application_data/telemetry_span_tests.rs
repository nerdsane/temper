use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use temper_authz::SecurityContext;
use temper_wasm_sdk::data::{DataOperationKind, DataOperationV1, ModuleDataErrorKind};
use tracing::Subscriber;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing_subscriber::layer::Context;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{Layer, registry::LookupSpan};

use super::tests::{call, invocation, response_error};

type CapturedSpan = (String, BTreeMap<String, String>);

#[derive(Clone, Default)]
struct CapturedSpans(Arc<Mutex<Vec<CapturedSpan>>>);
#[derive(Clone, Copy)]
struct CapturedSpanIndex(usize);

impl<S> Layer<S> for CapturedSpans
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut visitor = FieldVisitor::default();
        attrs.record(&mut visitor);
        let mut spans = self.0.lock().unwrap();
        let index = spans.len();
        spans.push((attrs.metadata().name().into(), visitor.0));
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(CapturedSpanIndex(index));
        }
    }
    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let Some(index) = span.extensions().get::<CapturedSpanIndex>().copied() else {
            return;
        };
        let mut visitor = FieldVisitor::default();
        values.record(&mut visitor);
        self.0.lock().unwrap()[index.0].1.extend(visitor.0);
    }
}

#[derive(Default)]
struct FieldVisitor(BTreeMap<String, String>);
impl Visit for FieldVisitor {
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.insert(field.name().into(), value.to_string());
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0.insert(field.name().into(), value.to_string());
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().into(), value.into());
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().into(), format!("{value:?}"));
    }
}

#[tokio::test]
async fn sdk_call_span_records_adapter_operation_result_and_consistency() {
    let invocation = invocation(
        BTreeSet::from([DataOperationKind::EntityGet]),
        SecurityContext::system(),
    );
    let captured = CapturedSpans::default();
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(captured.clone()));
    let response = call(
        &invocation,
        DataOperationV1::EntityGet {
            entity_type: "Temper.Example.Customer".into(),
            entity_id: "018f1f80-7b2d-7000-8000-000000000099".into(),
            at_least_sequence: Some(7),
        },
    )
    .await;
    assert_eq!(response_error(response).kind, ModuleDataErrorKind::NotFound);
    drop(_guard);
    let spans = captured.0.lock().unwrap();
    let fields = &spans
        .iter()
        .find(|(name, _)| name == "call_encoded")
        .expect("call span")
        .1;
    for (field, expected) in [
        ("abi_version", "1"),
        ("adapter", "module_sdk"),
        ("operation_kind", "entity_get"),
        ("entity_type", "Temper.Example.Customer"),
        ("result_kind", "error"),
        ("consistency_path", "authoritative"),
        ("outcome", "error"),
    ] {
        assert_eq!(
            fields.get(field).map(String::as_str),
            Some(expected),
            "{field}"
        );
    }
}
