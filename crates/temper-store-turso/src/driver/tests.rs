use super::*;

#[tokio::test]
async fn driver_futures_remain_send_and_lazy() {
    fn send<T: Send>(value: T) -> T {
        value
    }

    // Construction alone must not open a database or start network I/O.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("unpolled.db");
    drop(send(Database::local(path.to_str().unwrap())));
    assert!(!path.exists());
    drop(send(Database::remote("https://db.invalid", "unused")));

    let db = send(Database::local(":memory:")).await.unwrap();
    let conn = db.connect().unwrap();
    send(conn.execute("CREATE TABLE lazy_values(id INTEGER PRIMARY KEY)", ()))
        .await
        .unwrap();
    drop(send(conn.execute("INSERT INTO lazy_values VALUES (1)", ())));
    drop(send(conn.query("PRAGMA user_version=17", ())));
    drop(send(conn.begin_immediate()));

    let mut rows = send(conn.query("SELECT COUNT(*) FROM lazy_values", ()))
        .await
        .unwrap();
    assert_eq!(
        send(rows.next())
            .await
            .unwrap()
            .unwrap()
            .get::<i64>(0)
            .unwrap(),
        0
    );
    let mut rows = send(conn.query("PRAGMA user_version", ())).await.unwrap();
    // Dropping an unpolled next() must not consume the prefetched first row.
    drop(send(rows.next()));
    assert_eq!(
        send(rows.next())
            .await
            .unwrap()
            .unwrap()
            .get::<i64>(0)
            .unwrap(),
        0
    );
    drop(rows);

    let tx = send(conn.begin_immediate()).await.unwrap();
    drop(send(tx.execute("INSERT INTO lazy_values VALUES (2)", ())));
    drop(send(tx.query(
        "INSERT INTO lazy_values VALUES (3) RETURNING id",
        (),
    )));
    let mut rows = send(tx.query("SELECT COUNT(*) FROM lazy_values", ()))
        .await
        .unwrap();
    assert_eq!(
        send(rows.next())
            .await
            .unwrap()
            .unwrap()
            .get::<i64>(0)
            .unwrap(),
        0
    );
    drop(rows);
    send(tx.execute("INSERT INTO lazy_values VALUES (4)", ()))
        .await
        .unwrap();
    send(tx.commit()).await.unwrap();

    let tx = send(conn.begin_immediate()).await.unwrap();
    send(tx.execute("INSERT INTO lazy_values VALUES (5)", ()))
        .await
        .unwrap();
    send(tx.rollback()).await.unwrap();
    let mut rows = send(conn.query("SELECT id FROM lazy_values", ()))
        .await
        .unwrap();
    assert_eq!(
        send(rows.next())
            .await
            .unwrap()
            .unwrap()
            .get::<i64>(0)
            .unwrap(),
        4
    );
    assert!(send(rows.next()).await.unwrap().is_none());
}

#[tokio::test]
async fn local_connection_waits_for_a_contended_write_lock() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("contention.db");
    let db = Database::local(path.to_str().unwrap()).await.unwrap();
    let owner = db.connect().unwrap();
    owner
        .execute("CREATE TABLE committed_values(id INTEGER PRIMARY KEY)", ())
        .await
        .unwrap();
    let tx = owner.begin_immediate().await.unwrap();
    tx.execute("INSERT INTO committed_values VALUES (1)", ())
        .await
        .unwrap();

    let contender = db.connect().unwrap();
    let write = contender.execute("INSERT INTO committed_values VALUES (2)", ());
    tokio::pin!(write);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(25), &mut write)
            .await
            .is_err(),
        "a contended write must wait for the owning transaction"
    );
    tx.commit().await.unwrap();
    assert_eq!(write.await.unwrap(), 1);
    let mut rows = contender
        .query("SELECT COUNT(*) FROM committed_values", ())
        .await
        .unwrap();
    assert_eq!(
        rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap(),
        2
    );
}

#[tokio::test]
async fn local_rows_preserve_positional_and_named_values() {
    let db = Database::local(":memory:").await.unwrap();
    let conn = db.connect().unwrap();
    let mut rows = conn
        .query(
            "SELECT ?1, ?2, ?3, ?4, ?5",
            params![i64::MAX, -1.25, "Δ state", vec![0_u8, 255, 17], Value::Null,],
        )
        .await
        .unwrap();
    let row = rows.next().await.unwrap().unwrap();
    assert_eq!(row.get::<i64>(0).unwrap(), i64::MAX);
    assert_eq!(row.get::<f64>(1).unwrap(), -1.25);
    assert_eq!(row.get::<String>(2).unwrap(), "Δ state");
    assert_eq!(row.get::<Vec<u8>>(3).unwrap(), vec![0, 255, 17]);
    assert!(matches!(row.get_value(4).unwrap(), Value::Null));
    assert!(rows.next().await.unwrap().is_none());
    let mut rows = conn
        .query("SELECT :value", [(":value", "named")])
        .await
        .unwrap();
    assert_eq!(
        rows.next()
            .await
            .unwrap()
            .unwrap()
            .get::<String>(0)
            .unwrap(),
        "named"
    );
}

#[tokio::test]
async fn failed_transaction_rolls_back_before_next_connection_use() {
    let db = Database::local(":memory:").await.unwrap();
    let conn = db.connect().unwrap();
    conn.execute("CREATE TABLE values_to_commit(id INTEGER PRIMARY KEY)", ())
        .await
        .unwrap();
    {
        let tx = conn.begin_immediate().await.unwrap();
        tx.execute("INSERT INTO values_to_commit VALUES (1)", ())
            .await
            .unwrap();
        assert!(
            tx.execute("INSERT INTO values_to_commit VALUES (1)", ())
                .await
                .is_err()
        );
    }
    let mut rows = conn
        .query("SELECT COUNT(*) FROM values_to_commit", ())
        .await
        .unwrap();
    assert_eq!(
        rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap(),
        0
    );
    drop(rows);
    let tx = conn.begin_immediate().await.unwrap();
    tx.execute("INSERT INTO values_to_commit VALUES (2)", ())
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut rows = conn
        .query("SELECT id FROM values_to_commit", ())
        .await
        .unwrap();
    assert_eq!(
        rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap(),
        2
    );
}

#[tokio::test]
async fn query_executes_configuration_even_when_rows_are_discarded() {
    let db = Database::local(":memory:").await.unwrap();
    let conn = db.connect().unwrap();
    drop(conn.query("PRAGMA user_version=17", ()).await.unwrap());
    let mut rows = conn.query("PRAGMA user_version", ()).await.unwrap();
    assert_eq!(
        rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap(),
        17
    );
}

#[test]
fn remote_transient_errors_remain_retryable_without_retrying_permanent_errors() {
    for error in [
        turso_serverless::Error::Busy("database is locked".into()),
        turso_serverless::Error::BusySnapshot("snapshot conflict".into()),
        turso_serverless::Error::Http("request to https://db.invalid/v3/pipeline failed: error sending request for url (https://db.invalid/v3/pipeline)".into()),
        turso_serverless::Error::Http("cursor stream failed: error decoding response body".into()),
    ] {
        assert!(crate::retry::is_transient_write_error(
            &DriverError::from(error).to_string()
        ));
    }
    for error in [
        turso_serverless::Error::Constraint("UNIQUE constraint failed: proof.id".into()),
        turso_serverless::Error::Readonly("attempt to write a readonly database".into()),
        turso_serverless::Error::Http("HTTP status 401 Unauthorized".into()),
        turso_serverless::Error::Http("invalid pipeline response: missing field results".into()),
    ] {
        assert!(!crate::retry::is_transient_write_error(
            &DriverError::from(error).to_string()
        ));
    }
}

#[tokio::test]
async fn dropping_remote_connection_closes_its_transaction_stream() {
    use serde_json::json;
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v3/pipeline"))
        .and(body_partial_json(json!({"requests": [
            {"type": "execute", "stmt": {"sql": "BEGIN IMMEDIATE"}},
            {"type": "get_autocommit"}
        ]})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "baton": "transaction-stream", "base_url": null,
            "results": [
                {"type":"ok", "response":{"type":"execute", "result":{
                    "cols":[], "rows":[], "affected_row_count":0, "last_insert_rowid":null
                }}},
                {"type":"ok", "response":{"type":"get_autocommit", "is_autocommit":false}}
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let close = Mock::given(method("POST"))
        .and(path("/v3/pipeline"))
        .and(body_partial_json(json!({
            "baton":"transaction-stream", "requests":[{"type":"close"}]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "baton":null, "base_url":null,
            "results":[{"type":"ok", "response":{"type":"close"}}]
        })))
        .expect(1)
        .mount_as_scoped(&server)
        .await;

    let db = Database::remote(&server.uri(), "test-token").await.unwrap();
    let conn = db.connect().unwrap();
    let tx = conn.begin_immediate().await.unwrap();
    drop(tx);
    drop(conn);
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        close.wait_until_satisfied(),
    )
    .await
    .expect("dropping the connection must send Close, not wait for server expiry");
}
