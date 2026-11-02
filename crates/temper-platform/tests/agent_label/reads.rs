//! Identity on history and record reads.

use serde_json::Value;
use temper_store_sim::SimEventStore;

use super::fixture::{
    AGENT, KIND, LABEL, PERSON, act, agent, agent_labelled, event_names_the_agent, get, history,
    names_no_agent, person, read, record_names_the_agent, server, server_under,
};

#[tokio::test(flavor = "multi_thread")]
async fn an_agents_move_names_it_in_history_and_on_every_record_read() {
    let state = server(&SimEventStore::no_faults(1), 3600).await;
    act(&state, "j", "Touch", &agent()).await;

    let record = read(&state, "Jobs", "j").await;
    assert_eq!(record["last_event_via"], "request", "{record}");
    record_names_the_agent(&record);
    event_names_the_agent(history(&state, "Job", "j").await.last().unwrap());

    let listed = get(&state, "/tdata/Jobs", &agent()).await;
    let row = listed["value"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["entity_id"] == "j")
        .expect("the list holds the record");
    record_names_the_agent(row);

    let selected = get(
        &state,
        "/tdata/Jobs('j')?$select=last_event_for,last_event_agent_type,last_event_label",
        &agent(),
    )
    .await;
    assert_eq!(selected["last_event_for"], PERSON, "{selected}");
    assert_eq!(selected["last_event_agent_type"], KIND, "{selected}");
    assert_eq!(selected["last_event_label"], LABEL, "{selected}");

    for (filter, matches) in [
        (format!("last_event_label eq '{LABEL}'"), true),
        ("last_event_label eq 'someone-else'".to_string(), false),
        (format!("last_event_for eq '{PERSON}'"), true),
        (format!("last_event_agent_type eq '{KIND}'"), true),
    ] {
        let found = get(
            &state,
            &format!("/tdata/Jobs?$filter={}", filter.replace(' ', "%20")),
            &agent(),
        )
        .await;
        let held = found["value"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["entity_id"] == "j");
        assert_eq!(held, matches, "{filter}: {found}");
    }
}

/// A person's move names no agent, even after an agent's and even with a label
/// on the person's token.
#[tokio::test(flavor = "multi_thread")]
async fn a_persons_move_shows_no_agent_fields() {
    let state = server(&SimEventStore::no_faults(6), 3600).await;
    act(&state, "j", "Touch", &agent()).await;
    act(&state, "j", "Touch", &person()).await;

    let record = read(&state, "Jobs", "j").await;
    assert_eq!(record["last_event_by"], PERSON, "{record}");
    names_no_agent(
        &record,
        [
            "last_event_for",
            "last_event_agent_type",
            "last_event_label",
        ],
    );
    let moves = history(&state, "Job", "j").await;
    let event = moves.last().unwrap();
    assert_eq!(event["by"], PERSON, "{event}");
    names_no_agent(event, ["for", "agent_type", "label"]);
    event_names_the_agent(&moves[moves.len() - 2]);
}

/// An empty claim is no label, so a reader never gets an empty name.
#[tokio::test(flavor = "multi_thread")]
async fn an_empty_label_is_no_label() {
    let state = server(&SimEventStore::no_faults(7), 3600).await;
    act(&state, "j", "Touch", &agent_labelled("")).await;

    let record = read(&state, "Jobs", "j").await;
    assert_eq!(record["last_event_by"], AGENT, "{record}");
    assert_eq!(
        record.get("last_event_label"),
        Some(&Value::Null),
        "{record}"
    );
    let moves = history(&state, "Job", "j").await;
    assert_eq!(moves.last().unwrap().get("label"), Some(&Value::Null));
}

/// A label is a name: Cedar sees neither it nor the claim it came from.
#[tokio::test(flavor = "multi_thread")]
async fn a_label_is_not_an_authorization_input() {
    let policy = r#"
        permit(principal, action, resource);
        forbid(principal, action, resource)
        when {
            principal has label || principal has agent_label ||
            context has label || context has agent_label
        };
    "#;
    let state = server_under(&SimEventStore::no_faults(8), 3600, policy).await;
    act(&state, "j", "Touch", &agent()).await;
    assert_eq!(read(&state, "Jobs", "j").await["last_event_by"], AGENT);
}

/// With no query-plane store, collection rows must come from actors. Check
/// both full and selected lists against the single-record read contract.
#[tokio::test(flavor = "multi_thread")]
async fn actor_backed_lists_return_null_for_absent_agent_metadata() {
    let state = server(&SimEventStore::no_faults(9), 3600).await;
    assert!(
        state
            .server
            .storage_stack
            .as_ref()
            .expect("sim storage")
            .query_plane
            .is_none()
    );
    act(&state, "person", "Touch", &person()).await;
    act(&state, "unlabelled", "Touch", &agent_labelled("")).await;
    act(&state, "labelled", "Touch", &agent()).await;

    for uri in [
        "/tdata/Jobs",
        "/tdata/Jobs?$select=entity_id,last_event_by,last_event_for,last_event_agent_type,last_event_label,timer_needs_person",
    ] {
        let listed = get(&state, uri, &agent()).await;
        let rows = listed["value"].as_array().expect("list has rows");
        assert_eq!(rows.len(), 3, "{listed}");
        for id in ["person", "unlabelled", "labelled"] {
            let row = rows
                .iter()
                .find(|row| row["entity_id"] == id)
                .expect("the list holds the record");
            let single = read(&state, "Jobs", id).await;
            for property in [
                "last_event_by",
                "last_event_for",
                "last_event_agent_type",
                "last_event_label",
                "timer_needs_person",
            ] {
                assert_eq!(
                    row.get(property),
                    single.get(property),
                    "{uri}: {id}: {property}: {row}"
                );
                assert!(
                    row.get(property).is_some(),
                    "{uri}: {id}: {property}: {row}"
                );
            }
            if id == "person" {
                names_no_agent(
                    row,
                    [
                        "last_event_for",
                        "last_event_agent_type",
                        "last_event_label",
                    ],
                );
            } else if id == "unlabelled" {
                assert_eq!(row.get("last_event_label"), Some(&Value::Null), "{row}");
                assert_eq!(row["last_event_for"], PERSON, "{row}");
                assert_eq!(row["last_event_agent_type"], KIND, "{row}");
            } else {
                record_names_the_agent(row);
            }
        }
    }
}
