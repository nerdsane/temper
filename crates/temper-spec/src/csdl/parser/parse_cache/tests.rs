use std::panic::{AssertUnwindSafe, catch_unwind};

use super::*;
use crate::csdl::emit_csdl_xml;

const MODEL: &str = include_str!("../../../../../../test-fixtures/specs/model.csdl.xml");

fn local_cache(entries: usize, bytes: usize) -> Mutex<ParsedCsdlCache> {
    Mutex::new(ParsedCsdlCache::new(entries, bytes))
}

fn small_xml(namespace: &str) -> String {
    format!(
        r#"<Edmx Version="4.0"><Schema Namespace="{namespace}"><EntityType Name="Widget"><Property Name="Id" Type="Edm.String"/></EntityType></Schema></Edmx>"#
    )
}

fn assert_same_document(actual: &CsdlDocument, expected: &CsdlDocument) {
    assert_eq!(
        serde_json::to_value(actual).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
    assert_eq!(emit_csdl_xml(actual), emit_csdl_xml(expected));
}

#[test]
fn hits_preserve_complete_current_models_and_independent_owned_documents() {
    let cache = local_cache(8, TOTAL_SOURCE_BYTE_BUDGET);
    for source in [
        MODEL,
        include_str!("../../../../../../os-apps/project-management/specs/model.csdl.xml"),
        include_str!("../../../../../../os-apps/temper-fs/specs/model.csdl.xml"),
        include_str!("../../../../../../os-apps/agent-orchestration/specs/model.csdl.xml"),
        include_str!("../../../../../../crates/temper-platform/src/specs/model.csdl.xml"),
        include_str!("../../../../../../crates/temper-platform/src/specs/agent_model.csdl.xml"),
    ] {
        let fresh = parse_uncached(source).unwrap();
        let mut first = parse_with_cache(source, &cache).unwrap();
        let snapshot = cache.lock().unwrap().get(source).unwrap();
        let second = parse_with_cache(source, &cache).unwrap();
        assert_same_document(&first, &fresh);
        assert_same_document(&second, &fresh);

        first.version = "locally changed".into();
        first.schemas[0].namespace = "locally changed".into();
        first.schemas[0].entity_types.clear();
        first.schemas[0].actions.clear();
        assert!(Arc::ptr_eq(
            &snapshot,
            &cache.lock().unwrap().get(source).unwrap()
        ));
        assert_same_document(&parse_with_cache(source, &cache).unwrap(), &fresh);
    }
}

#[test]
fn exact_source_bytes_distinguish_formatting_and_changed_documents() {
    let cache = local_cache(3, 4096);
    let source = small_xml("First");
    let changed = small_xml("Changed");
    let formatted = format!("{source}\n<!-- same document, different source bytes -->");
    let first = parse_with_cache(&source, &cache).unwrap();
    assert_same_document(&parse_with_cache(&formatted, &cache).unwrap(), &first);
    assert_eq!(
        parse_with_cache(&changed, &cache).unwrap().schemas[0].namespace,
        "Changed"
    );
    let cache = cache.lock().unwrap();
    assert_eq!(cache.entries.len(), 3);
    assert_eq!(
        cache.source_bytes,
        source.len() + changed.len() + formatted.len()
    );
    assert!(!Arc::ptr_eq(
        &cache.get(&source).unwrap(),
        &cache.get(&formatted).unwrap()
    ));
}

#[test]
fn records_in_every_annotation_location_are_never_retained_or_evict_entries() {
    let cache = local_cache(1, 4096);
    let retained = small_xml("Retained");
    parse_with_cache(&retained, &cache).unwrap();
    let snapshot = cache.lock().unwrap().get(&retained).unwrap();
    for record in [
        "<Record/>",
        r#"<Record><PropertyValue Property="team" String="factory"/><PropertyValue Property="contact" String="owner"/></Record>"#,
    ] {
        let annotation = format!(r#"<Annotation Term="Test.Owner">{record}</Annotation>"#);
        for body in [
            annotation.clone(),
            format!(r#"<EntityType Name="Widget">{annotation}</EntityType>"#),
            format!(r#"<Action Name="Write">{annotation}</Action>"#),
            format!(r#"<Function Name="Read">{annotation}</Function>"#),
            format!(
                r#"<Annotations Target="Test.Widget" Qualifier="owner">{annotation}</Annotations>"#
            ),
        ] {
            let source = format!(r#"<Schema Namespace="Test">{body}</Schema>"#);
            let fresh = parse_uncached(&source).unwrap();
            assert!(contains_record_annotations(&fresh));
            for _ in 0..2 {
                let actual = parse_with_cache(&source, &cache).unwrap();
                assert_eq!(
                    serde_json::to_value(&actual).unwrap(),
                    serde_json::to_value(&fresh).unwrap()
                );
                let cache = cache.lock().unwrap();
                assert!(
                    cache.get(&source).is_none(),
                    "record source was retained: {source}"
                );
                assert!(Arc::ptr_eq(&snapshot, &cache.get(&retained).unwrap()));
                assert_eq!(cache.entries.len(), 1);
                assert_eq!(cache.source_bytes, retained.len());
                assert_eq!(cache.insertion_order.len(), 1);
            }
        }
    }
}

#[test]
fn floating_point_annotation_bits_survive_cache_hits() {
    let cache = local_cache(8, 4096);
    for literal in ["-0.0", "0.0", "NaN", "inf", "-inf", "1.25"] {
        let source = format!(
            r#"<Schema Namespace="Test"><Annotation Term="Test.Float" Float="{literal}"/></Schema>"#
        );
        let fresh = parse_uncached(&source).unwrap();
        let AnnotationValue::Float(expected) = fresh.schemas[0].annotations[0].value else {
            panic!("expected a floating point annotation");
        };
        if literal == "NaN" {
            assert!(expected.is_nan());
        }
        for _ in 0..2 {
            let actual = parse_with_cache(&source, &cache).unwrap();
            let AnnotationValue::Float(value) = actual.schemas[0].annotations[0].value else {
                panic!("expected a floating point annotation");
            };
            assert_eq!(value.to_bits(), expected.to_bits(), "literal={literal}");
            assert_same_document(&actual, &fresh);
        }
        assert!(cache.lock().unwrap().get(&source).is_some());
    }
}

#[test]
fn fifo_eviction_obeys_entry_budget_without_refreshing_hits() {
    let cache = local_cache(2, 4096);
    let first = small_xml("First");
    let second = small_xml("Second");
    let third = small_xml("Third");
    let owned = parse_with_cache(&first, &cache).unwrap();
    parse_with_cache(&second, &cache).unwrap();
    parse_with_cache(&first, &cache).unwrap();
    parse_with_cache(&third, &cache).unwrap();
    let cache = cache.lock().unwrap();
    assert!(cache.get(&first).is_none());
    assert!(cache.get(&second).is_some());
    assert!(cache.get(&third).is_some());
    assert_eq!(cache.entries.len(), 2);
    assert_eq!(cache.insertion_order.len(), 2);
    assert_eq!(cache.source_bytes, second.len() + third.len());
    assert_eq!(owned.schemas[0].namespace, "First");
}

#[test]
fn fifo_eviction_obeys_source_byte_budget_and_admits_exact_fits() {
    let first = small_xml("First");
    let second = small_xml("Second");
    let cache = local_cache(8, first.len() + second.len() - 1);
    parse_with_cache(&first, &cache).unwrap();
    parse_with_cache(&second, &cache).unwrap();
    let cache = cache.lock().unwrap();
    assert!(cache.get(&first).is_none());
    assert!(cache.get(&second).is_some());
    assert_eq!(cache.source_bytes, second.len());
    assert!(cache.source_bytes <= cache.source_byte_budget);

    let exact = local_cache(1, first.len());
    parse_with_cache(&first, &exact).unwrap();
    assert!(exact.lock().unwrap().get(&first).is_some());
}

#[test]
fn oversized_and_disabled_caches_return_the_fresh_document_without_retention() {
    let oversized = format!("<!--{}-->{MODEL}", "x".repeat(MAX_CACHED_SOURCE_BYTES));
    for (source, cache) in [
        (oversized.as_str(), local_cache(2, TOTAL_SOURCE_BYTE_BUDGET)),
        (MODEL, local_cache(0, TOTAL_SOURCE_BYTE_BUDGET)),
        (MODEL, local_cache(2, MODEL.len() - 1)),
    ] {
        let fresh = parse_uncached(source).unwrap();
        for _ in 0..2 {
            assert_same_document(&parse_with_cache(source, &cache).unwrap(), &fresh);
        }
        let cache = cache.lock().unwrap();
        assert!(cache.entries.is_empty());
        assert!(cache.insertion_order.is_empty());
        assert_eq!(cache.source_bytes, 0);
    }
}

#[test]
fn failed_parses_preserve_errors_and_do_not_insert_or_evict() {
    let cache = local_cache(1, 4096);
    let source = small_xml("Retained");
    parse_with_cache(&source, &cache).unwrap();
    let snapshot = cache.lock().unwrap().get(&source).unwrap();
    for invalid in [
        "<Schema></Schema>",
        r#"<Schema Namespace="Broken"><EntityType Name="Widget"></Schema>"#,
    ] {
        let expected = parse_uncached(invalid).unwrap_err().to_string();
        for _ in 0..2 {
            assert_eq!(
                parse_with_cache(invalid, &cache).unwrap_err().to_string(),
                expected
            );
        }
        let cache = cache.lock().unwrap();
        assert!(cache.get(invalid).is_none());
        assert!(Arc::ptr_eq(&snapshot, &cache.get(&source).unwrap()));
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(cache.insertion_order.len(), 1);
        assert_eq!(cache.source_bytes, source.len());
    }
}

#[test]
fn duplicate_insertions_reuse_the_original_without_double_accounting() {
    let source = small_xml("Shared");
    let mut cache = ParsedCsdlCache::new(1, 4096);
    let original = cache.insert(&source, Arc::new(parse_uncached(&source).unwrap()));
    let duplicate = cache.insert(&source, Arc::new(parse_uncached(&source).unwrap()));
    assert!(Arc::ptr_eq(&original, &duplicate));
    assert_eq!(cache.entries.len(), 1);
    assert_eq!(cache.insertion_order.len(), 1);
    assert_eq!(cache.source_bytes, source.len());
}

#[test]
fn poisoned_cache_is_bypassed_without_changing_documents_or_errors() {
    let cache = local_cache(1, TOTAL_SOURCE_BYTE_BUDGET);
    parse_with_cache(MODEL, &cache).unwrap();
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let _guard = cache.lock().unwrap();
        panic!("deliberately poison this test-local cache");
    }));
    assert!(cache.is_poisoned());
    assert_same_document(
        &parse_with_cache(MODEL, &cache).unwrap(),
        &parse_uncached(MODEL).unwrap(),
    );
    let changed = small_xml("Changed");
    assert_same_document(
        &parse_with_cache(&changed, &cache).unwrap(),
        &parse_uncached(&changed).unwrap(),
    );
    let invalid = "<Schema></Schema>";
    assert_eq!(
        parse_with_cache(invalid, &cache).unwrap_err().to_string(),
        parse_uncached(invalid).unwrap_err().to_string()
    );
}
