//! Each integration's callback has an idempotency key of its own.
//!
//! Two integrations on one action can answer with the same callback action and
//! the same parameters. Under a caller's `Idempotency-Key` both callbacks still
//! apply, and a retry of the request applies neither again, whether they run
//! inline or in the background, even when the outside service answers the
//! retry differently.

use std::time::Duration;

use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::ServerState;
use temper_server::idempotency::IdempotencyKey;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::state::DispatchExtOptions;
use temper_spec::csdl::parse_csdl;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SERVICE: &str = "sibling-callback-test";

const CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Siblings" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Survey">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Surveys" EntityType="Temper.Siblings.Survey"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

/// `Ask` calls two outside services; each answers with `Answered` and the same
/// result, and every `Answered` counts.
const SURVEY_IOA: &str = r#"
[automaton]
name = "Survey"
states = ["Idle", "Asking"]
initial = "Idle"

[[state]]
name = "answers"
type = "counter"
initial = 0

[[action]]
name = "Ask"
kind = "input"
from = ["Idle"]
to = "Asking"

[[action.triggers]]
name = "ask_first"
kind = "adapter"
adapter = "http"
on_success = "Answered"
on_failure = "Unanswered"
config = { method = "POST", url = "{url}/ask" }

[[action.triggers]]
name = "ask_second"
kind = "adapter"
adapter = "http"
on_success = "Answered"
on_failure = "Unanswered"
config = { method = "POST", url = "{url}/ask" }

[[action]]
name = "Answered"
kind = "input"
from = ["Asking"]
params = ["result"]
effect = ["answers += 1"]

[[action]]
name = "Unanswered"
kind = "input"
from = ["Asking"]
params = ["error_message"]
"#;

fn build_state(tenant: &str, survey_ioa: &str) -> ServerState {
    let mut registry = SpecRegistry::new();
    registry
        .try_register_tenant(
            tenant,
            parse_csdl(CSDL_XML).expect("CSDL should parse"),
            CSDL_XML.to_string(),
            &[("Survey", survey_ioa)],
        )
        .expect("tenant registration");
    let state = ServerState::from_registry(ActorSystem::new("sibling-callbacks"), registry);
    state
        .authz
        .reload_tenant_policies(
            tenant,
            r#"permit(principal == Agent::"service:sibling-callback-test", action, resource);"#,
        )
        .expect("policy should load");
    state.rebuild_reaction_dispatcher();
    state
}

#[tokio::test(flavor = "multi_thread")]
async fn two_integrations_answering_alike_both_apply_under_one_caller_key() {
    let outside = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/ask"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "callback_params": { "result": "yes" } })),
        )
        .mount(&outside)
        .await;
    let tenant = TenantId::new("surveys");
    let state = build_state("surveys", &SURVEY_IOA.replace("{url}", &outside.uri()));
    let who = AgentContext {
        idempotency_key: Some(IdempotencyKey::Request("ask-1".to_string())),
        ..AgentContext::for_service(SERVICE)
    };
    let ask = || {
        state.dispatch_tenant_action_ext(
            &tenant,
            "Survey",
            "s1",
            "Ask",
            serde_json::json!({}),
            DispatchExtOptions {
                agent_ctx: &who,
                await_integration: true,
                await_reactions: true,
            },
        )
    };
    let answers = || async {
        let entity = state
            .get_tenant_entity_state(&tenant, "Survey", "s1")
            .await
            .expect("record");
        entity.state.counters.get("answers").copied().unwrap_or(0)
    };

    let first = ask().await.expect("Ask dispatches");
    assert!(first.success, "{:?}", first.error);
    assert_eq!(
        answers().await,
        2,
        "each integration's callback applied, though both sent the same answer"
    );

    let retry = ask().await.expect("retried Ask dispatches");
    assert!(retry.success, "{:?}", retry.error);
    assert_eq!(answers().await, 2, "a retry applies neither callback again");
}

/// [`SURVEY_IOA`] calling `/first` and `/second`. The second has no failure
/// callback, so its failure goes to the record's failure path; every `Unanswered` counts in
/// `misses`, so one applied beside an `Answered` shows. Each callback reacts
/// on the same survey (`Tally` and `Flag`), so a reaction started for a
/// callback that did not commit shows too.
const TWO_SERVICES_IOA: &str = r#"
[automaton]
name = "Survey"
states = ["Idle", "Asking"]
initial = "Idle"

[[state]]
name = "answers"
type = "counter"
initial = 0

[[state]]
name = "misses"
type = "counter"
initial = 0

[[state]]
name = "tallies"
type = "counter"
initial = 0

[[state]]
name = "flags"
type = "counter"
initial = 0

[[action]]
name = "Ask"
kind = "input"
from = ["Idle"]
to = "Asking"

[[action.triggers]]
name = "ask_first"
kind = "adapter"
adapter = "http"
on_success = "Answered"
on_failure = "Unanswered"
config = { method = "POST", url = "{url}/first" }

[[action.triggers]]
name = "ask_second"
kind = "adapter"
adapter = "http"
on_success = "Answered"
config = { method = "POST", url = "{url}/second" }

[[action]]
name = "Answered"
kind = "input"
from = ["Asking"]
params = ["result"]
effect = ["answers += 1"]

[[action.triggers]]
name = "answer_tallies"
kind = "entity"
target_entity = "Survey"
target_action = "Tally"
resolve_target = { kind = "same_id" }

[[action]]
name = "Unanswered"
kind = "input"
from = ["Asking"]
params = ["error_message"]
effect = ["misses += 1"]

[[action.triggers]]
name = "miss_flags"
kind = "entity"
target_entity = "Survey"
target_action = "Flag"
resolve_target = { kind = "same_id" }

[[action]]
name = "Tally"
kind = "internal"
from = ["Asking"]
effect = ["tallies += 1"]

[[action]]
name = "Flag"
kind = "internal"
from = ["Asking"]
effect = ["flags += 1"]
"#;

/// Survey `s1` as one caller sees it, asking under one `Idempotency-Key`.
struct Survey {
    state: ServerState,
    tenant: TenantId,
    who: AgentContext,
}

impl Survey {
    /// A survey on [`TWO_SERVICES_IOA`] calling `outside`, asked under `key`.
    fn new(outside: &MockServer, key: &str) -> Self {
        Self {
            state: build_state(
                "surveys",
                &TWO_SERVICES_IOA.replace("{url}", &outside.uri()),
            ),
            tenant: TenantId::new("surveys"),
            who: AgentContext {
                idempotency_key: Some(IdempotencyKey::Request(key.to_string())),
                ..AgentContext::for_service(SERVICE)
            },
        }
    }

    /// `Ask`, answered once its integrations and their callbacks have run.
    async fn ask_inline(&self) -> (bool, Option<String>) {
        self.ask(true).await
    }

    async fn ask(&self, await_integration: bool) -> (bool, Option<String>) {
        let response = self.answer(await_integration).await;
        (response.success, response.error)
    }

    /// The answer to `Ask`, whole.
    async fn answer(&self, await_integration: bool) -> temper_server::EntityResponse {
        self.state
            .dispatch_tenant_action_ext(
                &self.tenant,
                "Survey",
                "s1",
                "Ask",
                serde_json::json!({}),
                DispatchExtOptions {
                    agent_ctx: &self.who,
                    await_integration,
                    await_reactions: true,
                },
            )
            .await
            .expect("Ask dispatches")
    }

    /// The survey's counter `name`.
    async fn count(&self, name: &str) -> usize {
        let entity = self
            .state
            .get_tenant_entity_state(&self.tenant, "Survey", "s1")
            .await
            .expect("record");
        entity.state.counters.get(name).copied().unwrap_or(0)
    }
}

/// An outside service where `/first` answers `yes` once and `then` after,
/// and `/second` fails its first call and answers `yes` after. A request's
/// first run commits `/first`'s callback and then fails on `/second`.
async fn failing_partway(then: ResponseTemplate) -> MockServer {
    let outside = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/first"))
        .respond_with(answer("yes"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&outside)
        .await;
    Mock::given(method("POST"))
        .and(path("/first"))
        .respond_with(then)
        .with_priority(2)
        .mount(&outside)
        .await;
    Mock::given(method("POST"))
        .and(path("/second"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&outside)
        .await;
    Mock::given(method("POST"))
        .and(path("/second"))
        .respond_with(answer("yes"))
        .with_priority(2)
        .mount(&outside)
        .await;
    outside
}

fn answer(result: &str) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .set_body_json(serde_json::json!({ "callback_params": { "result": result } }))
}

/// How many calls the outside service has had on `route`.
async fn calls(outside: &MockServer, route: &str) -> usize {
    outside
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|request| request.url.path() == route)
        .count()
}

/// A request whose second integration fails still committed: it answers
/// success and names the failed trigger, and the first callback applied. Its
/// retry under the same key gets that answer and calls no service again; the
/// failure was handed to the record's failure path, not back to the caller.
#[tokio::test(flavor = "multi_thread")]
async fn a_retry_after_a_trigger_failed_gets_the_committed_answer() {
    let outside = failing_partway(answer("changed")).await;
    let survey = Survey::new(&outside, "ask-2");

    let first = survey.answer(true).await;
    assert!(first.success, "{:?}", first.error);
    assert_eq!(
        first.trigger_failures.len(),
        1,
        "{:?}",
        first.trigger_failures
    );
    assert_eq!(
        survey.count("answers").await,
        1,
        "the first callback committed"
    );

    let retry = survey.answer(true).await;
    assert!(retry.success, "{:?}", retry.error);
    assert_eq!(retry.trigger_failures, first.trigger_failures);
    assert_eq!(
        (
            calls(&outside, "/first").await,
            calls(&outside, "/second").await
        ),
        (1, 1),
        "the retry called no service again"
    );
    assert_eq!(survey.count("answers").await, 1);
    assert_eq!(survey.count("misses").await, 0);
    assert_eq!(
        survey.count("tallies").await,
        1,
        "the committed success reacted once"
    );
    assert_eq!(survey.count("flags").await, 0);
}

/// A caller that hangs up does not end its request's integrations: they run
/// on. Its retry waits for them, as long as the outside service takes, and
/// answers with their outcome; it runs none of them again.
#[tokio::test(flavor = "multi_thread")]
async fn a_retry_after_a_hang_up_waits_for_the_integrations_still_running() {
    let outside = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/first"))
        .respond_with(answer("yes"))
        .mount(&outside)
        .await;
    // The second service takes 2 s to answer, so the caller hangs up first.
    let hold = Duration::from_secs(2);
    Mock::given(method("POST"))
        .and(path("/second"))
        .respond_with(answer("yes").set_delay(hold))
        .mount(&outside)
        .await;
    let survey = std::sync::Arc::new(Survey::new(&outside, "ask-5"));

    let hung_up = tokio::spawn({
        let survey = survey.clone();
        async move { survey.ask_inline().await }
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while calls(&outside, "/second").await == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the second outside call never started"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let held_from = tokio::time::Instant::now();
    assert_eq!(
        survey.count("answers").await,
        1,
        "the first callback committed"
    );
    hung_up.abort();
    assert!(hung_up.await.expect_err("aborted").is_cancelled());

    let (success, error) = survey.ask_inline().await;
    assert!(success, "{error:?}");
    assert_eq!(
        survey.count("answers").await,
        2,
        "the retry answered once the second callback had applied"
    );
    assert_eq!(
        (
            calls(&outside, "/first").await,
            calls(&outside, "/second").await
        ),
        (1, 1),
        "the retry ran no integration again"
    );
    assert!(
        held_from.elapsed() < hold + Duration::from_secs(5),
        "the retry waited for the held call, not a timeout: {:?}",
        held_from.elapsed()
    );
}
