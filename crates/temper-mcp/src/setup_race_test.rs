//! Interpose HTTP observations to reproduce a row winning between GET and create.

use super::*;
use axum::{Router, body::Body, extract::State, http::Request, response::Response};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Clone)]
struct Race {
    client: Client,
    base: String,
    collection: String,
    entity: String,
    conflict: bool,
    reads: Arc<AtomicUsize>,
    actions: Arc<AtomicUsize>,
}

async fn interpose(State(race): State<Race>, request: Request<Body>) -> Response {
    let path = request.uri().path();
    if request.method() == Method::GET
        && path == race.entity
        && race.reads.fetch_add(1, Ordering::SeqCst) == 0
    {
        // The real row is seeded, but this client's first observation predates it.
        return Response::builder().status(404).body(Body::empty()).unwrap();
    }
    if request.method() == Method::POST && path == race.collection && race.conflict {
        // Emulate create-only POST while leaving the real winning row untouched.
        return Response::builder().status(409).body(Body::empty()).unwrap();
    }
    if request.method() == Method::POST && path.contains("')/") {
        race.actions.fetch_add(1, Ordering::SeqCst);
    }
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, 1024 * 1024).await.unwrap();
    let response = race
        .client
        .request(parts.method, format!("{}{}", race.base, parts.uri))
        .headers(parts.headers)
        .body(bytes)
        .send()
        .await
        .unwrap();
    Response::builder()
        .status(response.status())
        .body(Body::from(response.bytes().await.unwrap()))
        .unwrap()
}

async fn check_race(credential: bool, conflict: bool, compatible: bool) {
    let fixture = Fixture::new().await;
    let identity = identity(&fixture);
    let (set, entity_type, id, expected) = if credential {
        fixture
            .create("AgentTypes", REQUESTER_TYPE, type_fields())
            .await;
        fixture.admin.ensure_type().await.unwrap();
        let fields = credential_fields(&identity);
        (
            "AgentCredentials",
            "AgentCredential",
            fields["key_hash"].as_str().unwrap().to_owned(),
            fields,
        )
    } else {
        (
            "AgentTypes",
            "AgentType",
            REQUESTER_TYPE.to_owned(),
            type_fields(),
        )
    };
    let mut winner = expected.clone();
    if !compatible {
        winner[if credential {
            "agent_instance_id"
        } else {
            "system_prompt"
        }] = json!("foreign");
    }
    fixture.create(set, &id, winner).await;
    let before = fixture.events(entity_type, &id).await;
    let original = fixture.entity(set, &id).await;
    let race = Race {
        client: Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap(),
        base: fixture.admin.base.clone(),
        collection: format!("/tdata/{set}"),
        entity: format!("/tdata/{set}('{id}')"),
        conflict,
        reads: Arc::new(AtomicUsize::new(0)),
        actions: Arc::new(AtomicUsize::new(0)),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new().fallback(interpose).with_state(race.clone());
    let proxy = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let admin = SetupAdmin::new(&base, "default", fixture.admin.key.clone()).unwrap();
    let result = if credential {
        admin.ensure_credential(&identity).await
    } else {
        admin.ensure_type().await
    };
    proxy.abort();
    assert_eq!(
        race.reads.load(Ordering::SeqCst),
        if conflict { 2 } else { 1 }
    );
    if compatible {
        result.unwrap();
        let expected_actions = usize::from(!credential || !conflict);
        assert_eq!(race.actions.load(Ordering::SeqCst), expected_actions);
        assert_eq!(
            fixture
                .events(entity_type, &id)
                .await
                .as_array()
                .unwrap()
                .len(),
            before.as_array().unwrap().len() + expected_actions
        );
        let entity = fixture.entity(set, &id).await;
        assert_eq!(entity["status"], "Active");
        assert_fields(&entity, &expected);
    } else {
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains(if credential {
                "unexpected bindings"
            } else {
                "incompatible"
            }),
            "{error}"
        );
        assert_eq!(race.actions.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.events(entity_type, &id).await, before);
        assert_eq!(fixture.entity(set, &id).await, original);
    }
}

#[tokio::test]
async fn foreign_create_winner_is_not_overwritten() {
    for credential in [false, true] {
        for conflict in [false, true] {
            check_race(credential, conflict, false).await;
        }
    }
}

#[tokio::test]
async fn matching_create_winner_resumes_after_success_or_conflict() {
    for credential in [false, true] {
        for conflict in [false, true] {
            check_race(credential, conflict, true).await;
        }
    }
}
