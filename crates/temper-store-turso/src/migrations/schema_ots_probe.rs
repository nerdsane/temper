use libsql::{Connection, params};
use temper_runtime::persistence::PersistenceError;

use super::schema_ots_trigger::validate_ots_audit_trigger_contracts;
use super::schema_snapshot::compatibility_error;
use super::schema_trigger::TriggerCapability;
use crate::store::ots::{
    ENQUEUE_OTS_TRAJECTORY_SQL, MARK_OTS_TRAJECTORY_FAILED_SQL, MARK_OTS_TRAJECTORY_PERSISTED_SQL,
    PERSIST_OTS_TRAJECTORY_SQL,
};

const OTS_PROBE_FAILURE: &str = "trigger probe failure";

#[derive(Debug, Eq, PartialEq)]
struct OtsProbeState {
    tenant: String,
    agent_id: String,
    session_id: String,
    outcome: String,
    turn_count: i64,
    data: String,
    persistence_status: String,
    persist_attempts: i64,
    last_error: Option<String>,
}

struct OtsProbeIdentity {
    persisted_id: String,
    queued_id: String,
    tenant: String,
    agent_id: String,
}

type ExpectedOtsProbeState<'a> = (
    &'a str,
    &'a str,
    i64,
    &'a str,
    &'a str,
    i64,
    Option<&'a str>,
);

pub(super) async fn validate_legacy_ots_triggers(
    connection: &Connection,
    triggers: &[(&str, &TriggerCapability)],
) -> Result<(), PersistenceError> {
    validate_ots_audit_trigger_contracts(connection, triggers).await?;
    let trigger_names = triggers.iter().map(|(name, _)| *name).collect::<Vec<_>>();
    connection
        .execute("SAVEPOINT temper_verify_ots_triggers", ())
        .await
        .map_err(|error| schema_query_error("start OTS trigger probe", error))?;
    let outcome = probe_ots_trigger_writes(connection).await;
    let rollback = connection
        .execute("ROLLBACK TO SAVEPOINT temper_verify_ots_triggers", ())
        .await;
    if let Err(error) = rollback {
        return Err(schema_query_error("roll back OTS trigger probe", error));
    }
    let release = connection
        .execute("RELEASE SAVEPOINT temper_verify_ots_triggers", ())
        .await;
    if let Err(error) = release {
        return Err(schema_query_error("release OTS trigger probe", error));
    }
    outcome.map_err(|error| {
        compatibility_error(format!(
            "table 'ots_trajectories' has executable trigger extension(s) {trigger_names:?} that reject a rollback-only production persist/enqueue/status-transition probe: {error}"
        ))
    })
}

async fn probe_ots_trigger_writes(connection: &Connection) -> Result<(), PersistenceError> {
    let mut identity_rows = connection
        .query(
            "SELECT lower(hex(randomblob(16))), lower(hex(randomblob(16))),
                    lower(hex(randomblob(16))), lower(hex(randomblob(16)))",
            (),
        )
        .await
        .map_err(|error| schema_query_error("generate OTS trigger probe ids", error))?;
    let row = identity_rows
        .next()
        .await
        .map_err(|error| schema_query_error("read OTS trigger probe id", error))?
        .ok_or_else(|| compatibility_error("OTS trigger probe id query returned no row".into()))?;
    let identity = OtsProbeIdentity {
        persisted_id: row
            .get::<String>(0)
            .map_err(|error| schema_query_error("decode persisted OTS probe id", error))?,
        queued_id: row
            .get::<String>(1)
            .map_err(|error| schema_query_error("decode queued OTS probe id", error))?,
        tenant: row
            .get::<String>(2)
            .map_err(|error| schema_query_error("decode OTS probe tenant", error))?,
        agent_id: row
            .get::<String>(3)
            .map_err(|error| schema_query_error("decode OTS probe agent", error))?,
    };
    drop(identity_rows);

    connection
        .execute(
            PERSIST_OTS_TRAJECTORY_SQL,
            params![
                identity.persisted_id.clone(),
                identity.tenant.clone(),
                identity.agent_id.clone(),
                "persist-session".to_string(),
                "persist-outcome".to_string(),
                1_i64,
                "{\"stage\":\"persist\"}".to_string(),
            ],
        )
        .await
        .map_err(|error| schema_query_error("probe OTS persisted insert", error))?;
    require_ots_probe_state(
        connection,
        &identity.persisted_id,
        expected_ots_probe_state(
            &identity,
            (
                "persist-session",
                "persist-outcome",
                1,
                "{\"stage\":\"persist\"}",
                "persisted",
                0,
                None,
            ),
        ),
        "persist",
    )
    .await?;

    connection
        .execute(
            PERSIST_OTS_TRAJECTORY_SQL,
            params![
                identity.persisted_id.clone(),
                identity.tenant.clone(),
                identity.agent_id.clone(),
                "persist-replacement-session".to_string(),
                "persist-replacement-outcome".to_string(),
                2_i64,
                "{\"stage\":\"persist-replacement\"}".to_string(),
            ],
        )
        .await
        .map_err(|error| schema_query_error("probe OTS persisted replacement", error))?;
    require_ots_probe_state(
        connection,
        &identity.persisted_id,
        expected_ots_probe_state(
            &identity,
            (
                "persist-replacement-session",
                "persist-replacement-outcome",
                2,
                "{\"stage\":\"persist-replacement\"}",
                "persisted",
                0,
                None,
            ),
        ),
        "persist replacement",
    )
    .await?;

    connection
        .execute(
            ENQUEUE_OTS_TRAJECTORY_SQL,
            params![
                identity.queued_id.clone(),
                identity.tenant.clone(),
                identity.agent_id.clone(),
                "queue-session-a".to_string(),
                "queue-outcome-a".to_string(),
                2_i64,
                "{\"stage\":\"enqueue-insert\"}".to_string(),
            ],
        )
        .await
        .map_err(|error| schema_query_error("probe OTS enqueue insert", error))?;
    require_ots_probe_state(
        connection,
        &identity.queued_id,
        expected_ots_probe_state(
            &identity,
            (
                "queue-session-a",
                "queue-outcome-a",
                2,
                "{\"stage\":\"enqueue-insert\"}",
                "queued",
                0,
                None,
            ),
        ),
        "enqueue insert",
    )
    .await?;

    connection
        .execute(
            ENQUEUE_OTS_TRAJECTORY_SQL,
            params![
                identity.queued_id.clone(),
                identity.tenant.clone(),
                identity.agent_id.clone(),
                "queue-session-b".to_string(),
                "queue-outcome-b".to_string(),
                3_i64,
                "{\"stage\":\"enqueue-conflict\"}".to_string(),
            ],
        )
        .await
        .map_err(|error| schema_query_error("probe OTS enqueue conflict update", error))?;
    require_ots_probe_state(
        connection,
        &identity.queued_id,
        expected_ots_probe_state(
            &identity,
            (
                "queue-session-b",
                "queue-outcome-b",
                3,
                "{\"stage\":\"enqueue-conflict\"}",
                "queued",
                0,
                None,
            ),
        ),
        "enqueue conflict update",
    )
    .await?;

    connection
        .execute(
            MARK_OTS_TRAJECTORY_FAILED_SQL,
            params![identity.queued_id.clone(), OTS_PROBE_FAILURE.to_string()],
        )
        .await
        .map_err(|error| schema_query_error("probe OTS failed status update", error))?;
    require_ots_probe_state(
        connection,
        &identity.queued_id,
        expected_ots_probe_state(
            &identity,
            (
                "queue-session-b",
                "queue-outcome-b",
                3,
                "{\"stage\":\"enqueue-conflict\"}",
                "failed",
                1,
                Some(OTS_PROBE_FAILURE),
            ),
        ),
        "failed status update",
    )
    .await?;

    connection
        .execute(
            MARK_OTS_TRAJECTORY_PERSISTED_SQL,
            params![identity.queued_id.clone()],
        )
        .await
        .map_err(|error| schema_query_error("probe OTS persisted status update", error))?;
    require_ots_probe_state(
        connection,
        &identity.queued_id,
        expected_ots_probe_state(
            &identity,
            (
                "queue-session-b",
                "queue-outcome-b",
                3,
                "{\"stage\":\"enqueue-conflict\"}",
                "persisted",
                1,
                None,
            ),
        ),
        "persisted status update",
    )
    .await?;
    Ok(())
}

fn expected_ots_probe_state(
    identity: &OtsProbeIdentity,
    expected: ExpectedOtsProbeState<'_>,
) -> OtsProbeState {
    let (session_id, outcome, turn_count, data, persistence_status, persist_attempts, last_error) =
        expected;
    OtsProbeState {
        tenant: identity.tenant.clone(),
        agent_id: identity.agent_id.clone(),
        session_id: session_id.to_string(),
        outcome: outcome.to_string(),
        turn_count,
        data: data.to_string(),
        persistence_status: persistence_status.to_string(),
        persist_attempts,
        last_error: last_error.map(str::to_string),
    }
}

async fn require_ots_probe_state(
    connection: &Connection,
    trajectory_id: &str,
    expected: OtsProbeState,
    stage: &str,
) -> Result<(), PersistenceError> {
    let mut rows = connection
        .query(
            "SELECT tenant, agent_id, session_id, outcome, turn_count, data,
                    persistence_status, persist_attempts, last_error
             FROM ots_trajectories WHERE trajectory_id = ?1",
            [trajectory_id],
        )
        .await
        .map_err(|error| schema_query_error("inspect OTS trigger probe state", error))?;
    let row = rows
        .next()
        .await
        .map_err(|error| schema_query_error("read OTS trigger probe state", error))?
        .ok_or_else(|| {
            compatibility_error(format!("OTS trigger probe row is missing after {stage}"))
        })?;
    let actual = OtsProbeState {
        tenant: row
            .get::<String>(0)
            .map_err(|error| schema_query_error("decode OTS probe tenant", error))?,
        agent_id: row
            .get::<String>(1)
            .map_err(|error| schema_query_error("decode OTS probe agent", error))?,
        session_id: row
            .get::<String>(2)
            .map_err(|error| schema_query_error("decode OTS probe session", error))?,
        outcome: row
            .get::<String>(3)
            .map_err(|error| schema_query_error("decode OTS probe outcome", error))?,
        turn_count: row
            .get::<i64>(4)
            .map_err(|error| schema_query_error("decode OTS probe turn count", error))?,
        data: row
            .get::<String>(5)
            .map_err(|error| schema_query_error("decode OTS probe data", error))?,
        persistence_status: row
            .get::<String>(6)
            .map_err(|error| schema_query_error("decode OTS probe status", error))?,
        persist_attempts: row
            .get::<i64>(7)
            .map_err(|error| schema_query_error("decode OTS probe attempts", error))?,
        last_error: row
            .get::<Option<String>>(8)
            .map_err(|error| schema_query_error("decode OTS probe error", error))?,
    };
    if actual != expected {
        return Err(compatibility_error(format!(
            "OTS trigger probe produced incompatible state after {stage}: expected {expected:?}, found {actual:?}"
        )));
    }
    Ok(())
}

fn schema_query_error(context: &str, error: libsql::Error) -> PersistenceError {
    PersistenceError::Storage(format!(
        "Turso schema introspection failed while attempting to {context}: {error} ({error:?})"
    ))
}
