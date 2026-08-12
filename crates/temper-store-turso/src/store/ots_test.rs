use super::*;

async fn test_store() -> (TursoEventStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("create temp dir");
    let db_path = dir.path().join("ots-outbox.db");
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso store");
    (store, dir)
}

fn params<'a>(trajectory_id: &'a str, data: &'a str) -> OtsTrajectoryParams<'a> {
    tenant_params("tenant", trajectory_id, data)
}

fn tenant_params<'a>(
    tenant: &'a str,
    trajectory_id: &'a str,
    data: &'a str,
) -> OtsTrajectoryParams<'a> {
    OtsTrajectoryParams {
        trajectory_id,
        tenant,
        agent_id: "agent",
        session_id: "session",
        outcome: "success",
        turn_count: 2,
        data,
    }
}

#[tokio::test]
async fn ots_outbox_status_lifecycle_is_durable() {
    let (store, _dir) = test_store().await;
    let data = r#"{"trajectory_id":"traj-durable","turns":[]}"#;

    store
        .enqueue_ots_trajectory(&params("traj-durable", data))
        .await
        .expect("enqueue trajectory");

    let rows = store
        .list_ots_trajectories("tenant", None, None, 10)
        .await
        .expect("list trajectories");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].persistence_status, "queued");

    let queued = store
        .list_queued_ots_trajectories(10)
        .await
        .expect("list queued trajectories");
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].trajectory_id, "traj-durable");
    assert_eq!(queued[0].data, data);

    store
        .mark_ots_trajectory_persisted("tenant", "traj-durable")
        .await
        .expect("mark persisted");
    let rows = store
        .list_ots_trajectories("tenant", None, None, 10)
        .await
        .expect("list persisted trajectory");
    assert_eq!(rows[0].persistence_status, "persisted");
    assert!(rows[0].last_error.is_none());

    store
        .enqueue_ots_trajectory(&params("traj-durable", data))
        .await
        .expect("requeue trajectory");
    store
        .mark_ots_trajectory_failed("tenant", "traj-durable", "transient")
        .await
        .expect("mark failed");
    let rows = store
        .list_ots_trajectories("tenant", None, None, 10)
        .await
        .expect("list failed trajectory");
    assert_eq!(rows[0].persistence_status, "failed");
    assert_eq!(rows[0].persist_attempts, 1);
    assert_eq!(rows[0].last_error.as_deref(), Some("transient"));
}

#[tokio::test]
async fn get_ots_trajectory_is_scoped_to_its_tenant() {
    let (store, _dir) = test_store().await;
    let data = r#"{"trajectory_id":"traj-tenant-a","turns":[]}"#;
    store
        .persist_ots_trajectory(&params("traj-tenant-a", data))
        .await
        .expect("persist trajectory");

    let document = store
        .get_ots_trajectory("tenant", "traj-tenant-a")
        .await
        .expect("read own tenant")
        .expect("document present");
    assert_eq!(document.data, data);
    assert_eq!(document.session_id, "session");
    assert_eq!(document.agent_id, "agent");
    assert!(
        store
            .get_ots_trajectory("other-tenant", "traj-tenant-a")
            .await
            .expect("read foreign tenant")
            .is_none(),
        "a foreign tenant must not read another tenant's trajectory by id"
    );
}

#[tokio::test]
async fn one_tenant_s_upload_cannot_replace_another_s_row_with_the_same_id() {
    // The trajectory id is chosen by the uploading harness, so two tenants
    // colliding on one is an ordinary event, not an attack.
    let (store, _dir) = test_store().await;
    let alpha = r#"{"trajectory_id":"traj-shared","turns":[],"owner":"alpha"}"#;
    let beta = r#"{"trajectory_id":"traj-shared","turns":[],"owner":"beta"}"#;

    store
        .persist_ots_trajectory(&tenant_params("alpha", "traj-shared", alpha))
        .await
        .expect("persist alpha");
    store
        .persist_ots_trajectory(&tenant_params("beta", "traj-shared", beta))
        .await
        .expect("persist beta");

    let alpha_row = store
        .get_ots_trajectory("alpha", "traj-shared")
        .await
        .expect("read alpha")
        .expect("alpha still has its row");
    assert_eq!(
        alpha_row.data, alpha,
        "a second tenant's upload must not overwrite the first tenant's trajectory"
    );
    let beta_row = store
        .get_ots_trajectory("beta", "traj-shared")
        .await
        .expect("read beta")
        .expect("beta has its own row");
    assert_eq!(beta_row.data, beta);
}

#[tokio::test]
async fn marking_one_tenant_s_trajectory_leaves_another_s_alone() {
    let (store, _dir) = test_store().await;
    let data = r#"{"trajectory_id":"traj-shared","turns":[]}"#;
    store
        .enqueue_ots_trajectory(&tenant_params("alpha", "traj-shared", data))
        .await
        .expect("enqueue alpha");
    store
        .enqueue_ots_trajectory(&tenant_params("beta", "traj-shared", data))
        .await
        .expect("enqueue beta");

    store
        .mark_ots_trajectory_failed("alpha", "traj-shared", "transient")
        .await
        .expect("mark alpha failed");

    let beta = store
        .list_ots_trajectories("beta", None, None, 10)
        .await
        .expect("list beta");
    assert_eq!(beta.len(), 1);
    assert_eq!(
        beta[0].persistence_status, "queued",
        "a status update addressed at one tenant must not land on another's row"
    );
    assert!(beta[0].last_error.is_none());
}

#[tokio::test]
async fn a_globally_keyed_table_is_rekeyed_by_tenant_on_open() {
    // Databases created before the identity fix carry a table keyed on
    // `trajectory_id` alone. Opening the store has to rekey it, keeping
    // the rows it already holds.
    let dir = tempfile::tempdir().expect("create temp dir");
    let db_path = dir.path().join("legacy-ots.db");
    let db_url = format!("file:{}", db_path.display());

    {
        let legacy = libsql::Builder::new_local(&db_path)
            .build()
            .await
            .expect("open legacy db");
        let conn = legacy.connect().expect("connect legacy db");
        conn.execute(
            "CREATE TABLE ots_trajectories (
                    trajectory_id TEXT PRIMARY KEY,
                    tenant TEXT NOT NULL,
                    agent_id TEXT NOT NULL,
                    session_id TEXT,
                    outcome TEXT NOT NULL DEFAULT 'unknown',
                    entity_type TEXT,
                    turn_count INTEGER NOT NULL DEFAULT 0,
                    data TEXT NOT NULL,
                    persistence_status TEXT NOT NULL DEFAULT 'persisted',
                    persist_attempts INTEGER NOT NULL DEFAULT 0,
                    last_error TEXT,
                    created_at TEXT NOT NULL DEFAULT (datetime('now')),
                    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
                );",
            (),
        )
        .await
        .expect("create legacy table");
        conn.execute(
            "INSERT INTO ots_trajectories \
                 (trajectory_id, tenant, agent_id, session_id, outcome, turn_count, data) \
                 VALUES ('traj-legacy', 'alpha', 'agent', 'session', 'success', 1, '{}')",
            (),
        )
        .await
        .expect("seed legacy row");
    }

    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("open the store, running the rekey");

    assert!(
        store
            .get_ots_trajectory("alpha", "traj-legacy")
            .await
            .expect("read the carried-over row")
            .is_some(),
        "the rekey must carry existing rows across"
    );
    store
        .persist_ots_trajectory(&tenant_params(
            "beta",
            "traj-legacy",
            "{\"owner\":\"beta\"}",
        ))
        .await
        .expect("a second tenant can now hold the same id");
    assert_eq!(
        store
            .get_ots_trajectory("alpha", "traj-legacy")
            .await
            .expect("read alpha")
            .expect("alpha kept its row")
            .data,
        "{}",
        "the rekeyed table must not let one tenant clobber another"
    );
}
