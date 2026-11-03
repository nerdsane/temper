//! A tenant's WASM invocation log keeps its newest rows, up to the bound its
//! only reader reads, and pruning one tenant leaves the others alone.

use temper_runtime::persistence::WASM_INVOCATIONS_KEPT_PER_TENANT;

use super::make_store;
use crate::TursoWasmInvocationInsert;

fn invocation<'a>(tenant: &'a str, entity_id: &'a str) -> TursoWasmInvocationInsert<'a> {
    TursoWasmInvocationInsert {
        tenant,
        entity_type: "Job",
        entity_id,
        module_name: "poll",
        trigger_action: "Poll",
        callback_action: None,
        success: true,
        error: None,
        duration_ms: 1,
        created_at: "2026-11-02T00:00:00Z",
    }
}

#[tokio::test]
async fn a_tenants_invocation_log_keeps_its_newest_rows_up_to_the_bound() {
    let store = make_store("wasm-invocation-bound").await;
    for id in ["q-0", "q-1", "q-2"] {
        store
            .persist_wasm_invocation(&invocation("quiet", id))
            .await
            .unwrap();
    }
    let busy = WASM_INVOCATIONS_KEPT_PER_TENANT + 250;
    for n in 0..busy {
        let id = format!("e-{n}");
        store
            .persist_wasm_invocation(&invocation("busy", &id))
            .await
            .unwrap();
    }
    let rows = store
        .load_recent_wasm_invocations("busy", 2 * busy)
        .await
        .unwrap();
    let kept: Vec<&str> = rows
        .iter()
        .filter(|row| row.tenant == "busy")
        .map(|row| row.entity_id.as_str())
        .collect();
    let kept_count = kept.len() as i64;
    assert_eq!(
        kept_count, WASM_INVOCATIONS_KEPT_PER_TENANT,
        "the busy tenant keeps exactly the newest rows up to the bound"
    );
    let newest = format!("e-{}", busy - 1);
    assert!(kept.contains(&newest.as_str()), "its newest row is kept");
    assert!(!kept.contains(&"e-0"), "its oldest rows are pruned");
    let quiet = store
        .load_recent_wasm_invocations("quiet", 2 * busy)
        .await
        .unwrap();
    assert_eq!(
        quiet.len(),
        3,
        "another tenant's rows are left alone"
    );
}

#[tokio::test]
async fn interleaved_tenants_each_keep_their_newest_invocations() {
    let store = make_store("wasm-invocation-interleaved").await;
    let inserted = WASM_INVOCATIONS_KEPT_PER_TENANT + 250;
    for n in 0..inserted {
        for tenant in ["odd", "even"] {
            store
                .persist_wasm_invocation(&invocation(tenant, &format!("e-{n}")))
                .await
                .unwrap();
        }
    }
    for tenant in ["odd", "even"] {
        let rows = store
            .load_recent_wasm_invocations(tenant, 2 * inserted)
            .await
            .unwrap();
        let mut ids: Vec<i64> = rows
            .iter()
            .filter(|row| row.tenant == tenant)
            .map(|row| row.entity_id.strip_prefix("e-").unwrap().parse().unwrap())
            .collect();
        assert_eq!(
            ids.len() as i64,
            WASM_INVOCATIONS_KEPT_PER_TENANT,
            "{tenant} retains a bounded log"
        );
        ids.sort();
        assert_eq!(
            ids,
            (inserted - WASM_INVOCATIONS_KEPT_PER_TENANT..inserted).collect::<Vec<_>>(),
            "every tenant retains exactly its newest rows, regardless of global id parity"
        );
    }
}

/// An existing backlog must drain without monopolizing the shared writer.
#[tokio::test]
async fn a_journal_append_can_land_between_log_cleanup_batches() {
    use super::super::write_gate::WritePriority;
    use super::{EventStore, test_envelope};
    use crate::driver::params;

    let store = make_store("wasm-invocation-backlog").await;
    let backlog = WASM_INVOCATIONS_KEPT_PER_TENANT + 999;
    let conn = store
        .write_connection("test.seed_wasm_logs", WritePriority::Low)
        .await
        .unwrap();
    let tx = conn.begin_immediate().await.unwrap();
    for n in 0..backlog {
        tx.execute(
            "INSERT INTO wasm_invocation_logs \
             (tenant, entity_type, entity_id, module_name, trigger_action, duration_ms) \
             VALUES ('backlog', 'Job', ?1, 'poll', 'Poll', 1)",
            params![n.to_string()],
        )
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
    drop(conn);

    let newest = invocation("backlog", "newest");
    let (pruned, remaining) = tokio::join!(store.persist_wasm_invocation(&newest), async {
        store
            .append(
                "backlog:Job:job-1",
                0,
                &[test_envelope("Created", serde_json::json!({}))],
            )
            .await
            .unwrap();
        store
            .load_recent_wasm_invocations("backlog", backlog + 1)
            .await
            .unwrap()
            .len() as i64
    },);
    pruned.unwrap();
    assert!(
        remaining > WASM_INVOCATIONS_KEPT_PER_TENANT,
        "the high-priority journal append finishes while cleanup still has work: {remaining}"
    );
    let rows = store
        .load_recent_wasm_invocations("backlog", backlog + 1)
        .await
        .unwrap();
    assert_eq!(rows.len() as i64, WASM_INVOCATIONS_KEPT_PER_TENANT);
    assert!(rows.iter().any(|row| row.entity_id == "newest"));
}
