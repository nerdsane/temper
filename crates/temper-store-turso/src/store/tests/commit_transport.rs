//! A remote COMMIT can lose its response before the server publishes its rows.
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use serde_json::{Value, json};
use temper_runtime::persistence::{EventStore, PersistenceAppend, PersistenceError};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate, matchers::method};

use super::{make_store, test_envelope};

#[derive(Clone)]
struct DelayedCommit {
    published: Arc<AtomicBool>,
    commits: Arc<AtomicUsize>,
    begins: Arc<AtomicUsize>,
    inserts: Arc<AtomicUsize>,
    events: [temper_runtime::persistence::PersistenceEnvelope; 2],
}

impl Respond for DelayedCommit {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = request.body_json().unwrap();
        if request.url.path() == "/v3/cursor" {
            let sql = body["batch"]["steps"][0]["stmt"]["sql"].as_str().unwrap();
            let journal = sql.starts_with("SELECT sequence_nr, event_type, payload, metadata");
            let columns = if journal {
                vec!["sequence_nr", "event_type", "payload", "metadata"]
            } else {
                vec!["value"]
            };
            let mut lines = vec![
                json!({"baton":"stream", "base_url":null}),
                json!({"type":"step_begin", "step":0, "cols":columns.iter().map(|name| json!({"name":name,"decltype":null})).collect::<Vec<_>>()}),
            ];
            if journal {
                if self.published.load(Ordering::SeqCst) {
                    let entity_id = body["batch"]["steps"][0]["stmt"]["args"][2]["value"]
                        .as_str()
                        .unwrap();
                    let event = &self.events[match entity_id {
                        "job-1" => 0,
                        "job-2" => 1,
                        other => panic!("unexpected journal {other}"),
                    }];
                    lines.push(json!({"type":"row", "row":[
                        {"type":"integer","value":"1"},
                        {"type":"text","value":event.event_type},
                        {"type":"text","value":event.payload.to_string()},
                        {"type":"text","value":serde_json::to_string(&event.metadata).unwrap()}
                    ]}));
                }
            } else {
                lines.push(json!({"type":"row","row":[{"type":"integer","value":"0"}]}));
            }
            lines.push(json!({"type":"step_end","affected_row_count":0,"last_insert_rowid":null}));
            // The read connection is in autocommit; the write connection is not.
            if journal {
                lines.push(json!({"type":"step_begin","step":1,"cols":[]}));
                lines.push(
                    json!({"type":"step_end","affected_row_count":0,"last_insert_rowid":null}),
                );
            } else {
                lines.push(json!({"type":"step_skipped","step":1}));
            }
            return ResponseTemplate::new(200).set_body_string(
                lines
                    .iter()
                    .map(Value::to_string)
                    .collect::<Vec<_>>()
                    .join("\n")
                    + "\n",
            );
        }
        let requests = body["requests"].as_array().unwrap();
        let mut results = Vec::new();
        for request in requests {
            let response = match request["type"].as_str().unwrap() {
                "execute" => {
                    match request["stmt"]["sql"].as_str().unwrap() {
                        "BEGIN IMMEDIATE" => {
                            self.begins.fetch_add(1, Ordering::SeqCst);
                        }
                        "COMMIT" => {
                            self.commits.fetch_add(1, Ordering::SeqCst);
                            // Lose the response through a real transport error. The
                            // server's write remains pending until the test publishes it.
                            return ResponseTemplate::new(307).insert_header(
                                "Location",
                                "http://127.0.0.1:0/lost-commit-response",
                            );
                        }
                        sql if sql.starts_with("INSERT INTO events") => {
                            // Batch inserts carry seven values per event row.
                            let rows = request["stmt"]["args"].as_array().unwrap().len() / 7;
                            self.inserts.fetch_add(rows, Ordering::SeqCst);
                        }
                        _ => {}
                    }
                    json!({"type":"execute","result":{"cols":[],"rows":[],"affected_row_count":1,"last_insert_rowid":null}})
                }
                "get_autocommit" => json!({"type":"get_autocommit","is_autocommit":false}),
                // Accepting stream closure is not a barrier for the pending
                // commit. Its publication remains under the test's control.
                "close" => json!({"type":"close"}),
                other => panic!("unexpected request {other}"),
            };
            results.push(json!({"type":"ok","response":response}));
        }
        ResponseTemplate::new(200)
            .set_body_json(json!({"baton":"stream","base_url":null,"results":results}))
    }
}

#[tokio::test]
async fn a_transport_failure_before_remote_commit_finishes_stays_unknown_without_retry() {
    for batch in [false, true] {
        let server = MockServer::start().await;
        let fault = DelayedCommit {
            published: Arc::new(AtomicBool::new(false)),
            commits: Arc::new(AtomicUsize::new(0)),
            begins: Arc::new(AtomicUsize::new(0)),
            inserts: Arc::new(AtomicUsize::new(0)),
            events: [
                test_envelope("Created", json!({"job":1})),
                test_envelope("Created", json!({"job":2})),
            ],
        };
        Mock::given(method("POST"))
            .respond_with(fault.clone())
            .mount(&server)
            .await;
        let mut store = make_store("delayed-remote-commit").await;
        // Skip migrations in this protocol fault fixture. Every append and
        // recovery operation still runs through the production remote driver.
        store.db = Arc::new(
            crate::driver::Database::remote(&server.uri(), "fixture-token")
                .await
                .unwrap(),
        );
        store.is_remote = true;
        let id = "tenant-a:Job:job-1";
        let ids = if batch {
            vec![id, "tenant-a:Job:job-2"]
        } else {
            vec![id]
        };
        let result = if batch {
            store
                .append_batch(
                    &ids.iter()
                        .enumerate()
                        .map(|(index, id)| PersistenceAppend {
                            persistence_id: (*id).into(),
                            expected_sequence: 0,
                            events: vec![fault.events[index].clone()],
                            state_timer: None,
                            projection: None,
                        })
                        .collect::<Vec<_>>(),
                )
                .await
                .map(|_| ())
        } else {
            store
                .append(id, 0, std::slice::from_ref(&fault.events[0]))
                .await
                .map(|_| ())
        };
        assert!(
            matches!(&result, Err(PersistenceError::OutcomeUnknown(message)) if message.contains("transport failure")),
            "batch={batch}: {result:?}"
        );
        assert_eq!(fault.commits.load(Ordering::SeqCst), 1);
        assert_eq!(fault.inserts.load(Ordering::SeqCst), ids.len());
        assert_eq!(
            fault.begins.load(Ordering::SeqCst),
            1,
            "unknown outcomes must not retry"
        );
        for id in &ids {
            assert!(store.read_events(id, 0).await.unwrap().is_empty());
        }
        fault.published.store(true, Ordering::SeqCst);
        for (index, id) in ids.iter().enumerate() {
            let landed = store.read_events(id, 0).await.unwrap();
            assert_eq!(landed.len(), 1);
            assert_eq!(
                landed[0].metadata.event_id,
                fault.events[index].metadata.event_id
            );
            assert_eq!(landed[0].payload, fault.events[index].payload);
        }
    }
}
