//! Exercise sparse PostgreSQL reads after the catalog splits fields from state.
use super::*;
use temper_store_postgres::{PostgresEventStore, migration::run_migrations};

#[tokio::test]
async fn selected_fields_preserve_stored_values_before_filling_defaults() {
    let Some(url) = temper_test_resource::env_var("DATABASE_URL") else {
        return;
    };
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    run_migrations(&pool).await.unwrap();
    let mut state = ServerState::from_registry(
        ActorSystem::new("postgres-declared-fields"),
        SpecRegistry::new(),
    );
    state.set_storage_stack(StorageStack::from_postgres(PostgresEventStore::new(pool)));
    let tenant = format!("selected-fields-{}", temper_runtime::scheduler::sim_uuid());
    install(&state, &tenant, V1);
    for id in ["stored", "missing"] {
        let (status, body) =
            request(&state, &tenant, "POST", "/tdata/Toppings", json!({"id":id})).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    install(&state, &tenant, &format!("{V1}\n{ADDED}"));
    for (action, params) in [("Touch", json!({"label":"stored"})), ("Reset", json!({}))] {
        let (status, body) = request(
            &state,
            &tenant,
            "POST",
            &format!("/tdata/Toppings('stored')/Temper.{action}"),
            params,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    // A later declaration must never replace a persisted value, including zero/false.
    let changed = ADDED.replace("initial = 6", "initial = 21");
    install(&state, &tenant, &format!("{V1}\n{changed}"));
    let (status, selected) = request(
        &state,
        &tenant,
        "GET",
        "/tdata/Toppings?$select=entity_id,fields",
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{selected}");
    let rows = selected["value"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    for row in rows {
        let id = row["entity_id"].as_str().unwrap();
        let (status, full) = request(
            &state,
            &tenant,
            "GET",
            &format!("/tdata/Toppings('{id}')"),
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{full}");
        assert_eq!(row["fields"], full["fields"], "{id}: {selected}");
        assert_eq!(
            row.as_object().unwrap().len(),
            3,
            "only selected keys and @odata.id"
        );
        if id == "stored" {
            assert_eq!(row["fields"]["label"], "stored");
            assert_eq!(row["fields"]["stock"], 0);
            assert_eq!(row["fields"]["available"], false);
        } else {
            assert_eq!(row["fields"]["label"], "fresh");
            assert_eq!(row["fields"]["stock"], 21);
            assert_eq!(row["fields"]["available"], true);
        }
    }
}
