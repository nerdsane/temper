use super::*;
use std::future::{Future, poll_fn};
use std::task::Poll;
use std::time::Duration;

#[test]
fn queued_validations_leave_blocking_capacity_for_the_active_check() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(2)
        .build()
        .unwrap();
    runtime.block_on(async {
        let app = build_router(test_state_with_registry());
        let turn = temper_server::host_turn::take().await.unwrap();
        let mut requests = Vec::new();
        for _ in 0..4 {
            let mut request = Box::pin(
                app.clone().oneshot(authenticated_post(
                    "/api/specs/validate-ioa",
                    &serde_json::json!({
                        "ioa_source": ORDER_IOA,
                        "sim_seeds": 1,
                        "prop_test_cases": 1
                    })
                    .to_string(),
                )),
            );
            // Drive each HTTP request into the occupied host turn. With
            // blocking lock waits, these fill the two-thread pool.
            poll_fn(|cx| {
                assert!(request.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            requests.push(request);
        }
        drop(turn);

        // The first admitted check needs blocking tasks for its digest and
        // cascade. The other requests must leave capacity for it to finish.
        let responses = tokio::time::timeout(
            Duration::from_secs(10),
            futures_util::future::join_all(requests),
        )
        .await
        .expect("queued validation requests starved the active check");
        for response in responses {
            let response = response.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(json["all_passed"], true, "{json}");
        }
    });
}
