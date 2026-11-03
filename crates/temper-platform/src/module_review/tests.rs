use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use wiremock::matchers::{header, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::source::{self, lines_of, without_comments};
use super::*;

/// A judge that rates one item's statement high and points at the first line holding `marker`.
struct Stub {
    found: Option<&'static str>,
    marker: &'static str,
    unavailable: Option<JudgeUnavailable>,
    calls: AtomicUsize,
}

impl Stub {
    fn finding(kind: &'static str, marker: &'static str) -> Self {
        Self {
            found: Some(kind),
            marker,
            unavailable: None,
            calls: AtomicUsize::new(0),
        }
    }

    fn clean() -> Self {
        Self {
            found: None,
            marker: "",
            unavailable: None,
            calls: AtomicUsize::new(0),
        }
    }

    fn down(reason: JudgeUnavailable) -> Self {
        Self {
            found: None,
            marker: "",
            unavailable: Some(reason),
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl Judge for Stub {
    async fn ask(
        &self,
        _module: &str,
        _state: &str,
        questions: &BTreeMap<String, Question>,
    ) -> Result<Answers, JudgeUnavailable> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(reason) = &self.unavailable {
            return Err(reason.clone());
        }
        let found = self
            .found
            .and_then(|kind| ITEMS.iter().find(|item| item.kind == kind));
        let answers = questions
            .iter()
            .map(|(key, question)| {
                let answer = match question {
                    Question::Noul { instructions } => {
                        let hit = found.is_some_and(|item| item.shows == instructions);
                        Answer {
                            noul: Some(if hit { 0.85 } else { 0.1 }),
                            choice: None,
                        }
                    }
                    Question::Choice { criteria, .. } => Answer {
                        noul: None,
                        choice: criteria
                            .iter()
                            .find(|(_, text)| text.contains(self.marker))
                            .map(|(key, _)| key.clone()),
                    },
                };
                (key.clone(), answer)
            })
            .collect();
        Ok(Answers {
            answers,
            input_tokens: 100,
        })
    }
}

fn specs(module: &str) -> Vec<SpecSource> {
    let spec = format!(
        r#"
[automaton]
name = "Order"
states = ["Open", "Done"]
initial = "Open"

[[action]]
name = "Advance"
from = ["Open"]

[[action.triggers]]
kind = "wasm"
module = "{module}"
on_success = "Advanced"
"#
    );
    vec![SpecSource::new("Order", &spec)]
}

#[test]
fn test_spec_source_reads_which_action_runs_which_module() {
    let spec = &specs("steps")[0];
    assert_eq!(
        spec.module_triggers,
        vec![("Advance".to_string(), "steps".to_string())]
    );
    assert_eq!(
        SpecSource::new("Broken", "not = [toml").module_triggers,
        Vec::new()
    );
}

fn module(name: &str, source: &str) -> ModuleSource {
    ModuleSource {
        module: name.to_string(),
        path: format!("wasm/{name}/src/lib.rs"),
        source: source.to_string(),
    }
}

const PHASED: &str = r#"fn run(ctx: &Ctx) -> Value {
    // walks its own steps
    let phase = ctx.field("phase");
    let total = ctx.field("total");
    let label = format!("{total} items");
    let note = "// not a comment";
    if phase == "fetch" {
        return json!({ "phase": "parse", "label": label, "note": note });
    }
    json!({ "phase": "done" })
}
"#;

#[test]
fn test_without_comments_keeps_lines_and_strings() {
    let source = "let a = 1; // gone\n/* gone\n   too */ let b = \"// kept\";\nlet c = '/';\n";
    let stripped = without_comments(source);
    assert_eq!(stripped.matches('\n').count(), source.matches('\n').count());
    assert!(!stripped.contains("gone"));
    assert!(stripped.contains("\"// kept\""));
    assert!(stripped.contains("'/'"));
    let lines = lines_of(source);
    assert_eq!(
        (lines[3].number, lines[3].text.as_str()),
        (4, "let c = '/';")
    );
}

#[tokio::test]
async fn test_review_names_item_line_and_runner() {
    let judge = Stub::finding("hidden_states", "ctx.field(\"phase\")");
    let review = review_module(&judge, &module("steps", PHASED), &specs("steps")).await;
    let review = review.expect("the stub answers");
    assert_eq!(review.findings.len(), 1);
    let finding = &review.findings[0];
    assert_eq!(finding.item, 2);
    assert_eq!(finding.location, "wasm/steps/src/lib.rs:3");
    assert_eq!(
        finding.line.as_deref(),
        Some("let phase = ctx.field(\"phase\");")
    );
    assert!(
        finding
            .why
            .starts_with("In this app the module runs from Order's Advance; ")
    );
    assert!(
        finding
            .instead
            .starts_with("Instead: make each phase a state")
    );
}

#[tokio::test]
async fn test_clean_module_has_no_findings() {
    let judge = Stub::clean();
    let review = review_module(&judge, &module("label", PHASED), &specs("label")).await;
    assert_eq!(review.expect("the stub answers").findings, Vec::new());
    assert_eq!(
        judge.calls.load(Ordering::SeqCst),
        1,
        "a clean module is one question set"
    );
}

#[tokio::test]
async fn test_large_module_is_flagged_by_size() {
    let body: String = (0..LARGE_LINES + 10)
        .map(|n| format!("    let v{n} = if n > {n} {{ 1 }} else {{ 0 }};\n"))
        .collect();
    let source = format!("fn run(n: i64) -> i64 {{\n{body}    0\n}}\n");
    let judge = Stub::clean();
    let review = review_module(&judge, &module("big", &source), &specs("big")).await;
    let findings = review.expect("the stub answers").findings;
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].kind, "large_module");
    assert_eq!(findings[0].item, 7);
    assert!(
        findings[0]
            .why
            .contains(&format!("with {} branches", LARGE_LINES + 10))
    );
    assert!(
        judge.calls.load(Ordering::SeqCst) >= 2,
        "a large module is read in windows"
    );
}

#[tokio::test]
async fn test_unavailable_judge_is_said_once_and_reviews_nothing() {
    let reviewer = ModuleReviewer::new(Arc::new(Stub::down(JudgeUnavailable::OutOfCredit)));
    let modules = [module("a", PHASED), module("b", PHASED)];
    let review = reviewer.review(&modules, &specs("a")).await;
    assert!(review.reviews.is_empty());
    assert_eq!(review.unavailable, Some(JudgeUnavailable::OutOfCredit));
}

#[tokio::test]
async fn test_reviewer_reuses_a_review_of_the_same_module_and_specs() {
    let judge = Arc::new(Stub::finding("hidden_states", "phase"));
    let reviewer = ModuleReviewer::new(judge.clone());
    let modules = [module("steps", PHASED)];
    let first = reviewer.review(&modules, &specs("steps")).await;
    let calls = judge.calls.load(Ordering::SeqCst);
    let again = reviewer.review(&modules, &specs("steps")).await;
    assert_eq!(first, again);
    assert_eq!(
        judge.calls.load(Ordering::SeqCst),
        calls,
        "a cached review asks nothing"
    );
    let changed = [module("steps", &PHASED.replace("total", "count"))];
    reviewer.review(&changed, &specs("steps")).await;
    assert!(
        judge.calls.load(Ordering::SeqCst) > calls,
        "changed source is read again"
    );
}

fn questions() -> BTreeMap<String, Question> {
    let mut questions = BTreeMap::new();
    questions.insert(
        "leaf".to_string(),
        Question::Noul {
            instructions: "x".to_string(),
        },
    );
    questions
}

#[tokio::test]
async fn test_jev_without_a_key_sends_nothing() {
    let server = MockServer::start().await;
    let judge = JevJudge::at(&server.uri(), None);
    assert_eq!(
        judge.ask("steps", "state", &questions()).await.unwrap_err(),
        JudgeUnavailable::NoKey
    );
    assert!(
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty()
    );
}

#[tokio::test]
async fn test_jev_out_of_credit_is_asked_once() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(402))
        .mount(&server)
        .await;
    let judge = JevJudge::at(&server.uri(), Some("k".to_string()));
    assert_eq!(
        judge.ask("steps", "state", &questions()).await.unwrap_err(),
        JudgeUnavailable::OutOfCredit
    );
    assert_eq!(
        server.received_requests().await.unwrap_or_default().len(),
        1
    );
}

#[tokio::test]
async fn test_jev_answer_is_read_with_its_tokens() {
    let server = MockServer::start().await;
    let body = serde_json::json!({
        "answers": { "leaf": { "noul": 0.9 } },
        "model": JEV_MODEL,
        "usage": { "input_tokens": 1234 }
    });
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer k"))
        .and(header("temper-review-module", "steps"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    let judge = JevJudge::at(&server.uri(), Some("k".to_string()));
    let answers = judge
        .ask("steps", "state", &questions())
        .await
        .expect("Jev answers");
    assert_eq!(answers.rating("leaf"), Some(0.9));
    assert_eq!(answers.input_tokens, 1234);
}

#[tokio::test]
async fn test_jev_down_is_tried_at_most_twice() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let judge = JevJudge::at(&server.uri(), Some("k".to_string()));
    let error = judge.ask("steps", "state", &questions()).await.unwrap_err();
    assert_eq!(
        error,
        JudgeUnavailable::Down("HTTP 503, HTTP 503".to_string())
    );
    assert_eq!(
        server.received_requests().await.unwrap_or_default().len(),
        2
    );
}

#[test]
fn test_jev_debug_never_shows_the_key() {
    let judge = JevJudge::new(Some("secret-key-value".to_string()));
    assert!(!format!("{judge:?}").contains("secret-key-value"));
}

/// An Order whose `Advance` runs `steps`: `Advanced` takes its answer, `Ship` leaves the state it reaches, `Archive`
/// is elsewhere, and `total` is a field none of those use.
fn order_spec() -> SpecSource {
    let spec = r#"
# The order's lifecycle.
[automaton]
name = "Order"
states = ["Open", "Waiting", "Ready", "Shipped", "Old"]
initial = "Open"

[[state]]
name = "attempts"
type = "counter"
initial = 0

[[state]]
name = "total"
type = "counter"
initial = 0

# Asks the module which way to go.
[[action]]
name = "Advance"
from = ["Open"]
to = "Waiting"
effect = ["attempts += 1"]

[[action.triggers]]
kind = "wasm"
module = "steps"
on_success = "Advanced"

[[action]]
name = "Advanced"
kind = "internal"
from = ["Waiting"]
to = "Ready"

[[action]]
name = "Ship"
from = ["Ready"]
to = "Shipped"

[[action]]
name = "Archive"
from = ["Shipped"]
to = "Old"
"#;
    SpecSource::new("Order", spec)
}

#[test]
fn test_excerpt_reads_what_runs_the_module_and_around_it_without_comments() {
    let read = excerpt("steps", &[order_spec()]);
    for kept in [
        "name = \"Advance\"",
        "name = \"Advanced\"",
        "name = \"Ship\"",
        "name = \"attempts\"",
        "states = [",
    ] {
        assert!(read.text.contains(kept), "{kept} is read");
    }
    for left in [
        "name = \"Archive\"",
        "name = \"total\"",
        "# Asks the module",
        "# The order's",
    ] {
        assert!(!read.text.contains(left), "{left} is not read");
    }
    assert!(read.left_out.is_empty(), "nothing it needs is left out");
    assert!(read.heading.contains("actions that run it"));
    // A module no action runs by name reads the whole spec.
    let whole = excerpt("other", &[order_spec()]);
    assert!(
        whole.text.contains("name = \"Archive\"")
            && whole.heading.contains("no action runs this module")
    );
}

/// The order spec with `Ship`, an action around the module, larger than all the room an excerpt has.
fn order_with_a_large_ship() -> SpecSource {
    let notes: String = (0..SPEC_BYTES / 150)
        .map(|n| format!("note{n} = \"{}\"\n", "x".repeat(150)))
        .collect();
    let spec = order_spec().source.replace(
        "[[action]]\nname = \"Ship\"\n",
        &format!("[[action]]\nname = \"Ship\"\n{notes}"),
    );
    SpecSource::new("Order", &spec)
}

#[test]
fn test_excerpt_past_its_bytes_leaves_out_the_farthest_and_names_it() {
    let read = excerpt("steps", &[order_with_a_large_ship()]);
    assert_eq!(read.left_out, vec!["Order's action Ship".to_string()]);
    assert!(read.text.contains("name = \"Advance\"") && read.text.contains("name = \"Advanced\""));
    assert!(read.text.len() <= SPEC_BYTES);
    assert_eq!(said_left_out(&read.left_out), "Order's action Ship");
    let many: Vec<String> = (1..=7).map(|n| format!("Order's action A{n}")).collect();
    assert_eq!(
        said_left_out(&many),
        "Order's action A1, Order's action A2, Order's action A3, Order's action A4, Order's action A5 and 2 more"
    );
}

#[tokio::test]
async fn test_review_says_what_it_left_out() {
    let judge = Stub::clean();
    let review = review_module(
        &judge,
        &module("steps", "pub fn run() {}\n"),
        &[order_with_a_large_ship()],
    )
    .await
    .expect("reviewed");
    assert!(review.findings.is_empty());
    assert_eq!(
        review.left_out,
        vec!["Order's action Ship".to_string()],
        "a clean review that read part of its spec says so"
    );
}

#[tokio::test]
async fn test_jev_route_answer_gives_its_tokens_at_the_top() {
    let server = MockServer::start().await;
    let body = serde_json::json!({ "answers": { "leaf": { "noul": 0.2 } }, "model": JEV_MODEL, "input_tokens": 77, "via": "jev" });
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    let answers = JevJudge::at(&server.uri(), Some("pass".to_string()))
        .ask("steps", "state", &questions())
        .await
        .expect("the route answers");
    assert_eq!(answers.input_tokens, 77);
}

#[test]
fn test_a_long_line_is_cut() {
    let line = format!("let x = \"{}\";", "y".repeat(400));
    let read = lines_of(&line);
    assert_eq!(read[0].text.chars().count(), source::LINE_CHARS + 1);
    assert!(read[0].text.ends_with('…'));
}
