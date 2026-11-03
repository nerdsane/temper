//! A carry's `then` move made for the user a record names (`as`): the
//! platform's carry service acts for them, so the move binds their identity
//! and the version's Cedar decides it for them, at install and when it is
//! made.

use super::*;
use temper_authz::PrincipalKind;

/// A place a user asks for, which binds who asked; `Bring` stores none.
const ASKED: &str = r#"
[automaton]
name = "Place"
states = ["Unplaced", "Bringing", "Placed"]
initial = "Unplaced"
allow_indefinite_states = ["Unplaced", "Bringing", "Placed"]
strict_action_params = true

[[state]]
name = "asked_by"
type = "string"
initial = ""

[[action]]
name = "Ask"
kind = "input"
from = ["Unplaced"]
to = "Bringing"
params = [{ name = "asked_by", type = "string", source = "authenticated_subject" }]

[[action]]
name = "Bring"
kind = "input"
from = ["Unplaced"]
to = "Bringing"
params = []

[[action]]
name = "Placed"
kind = "input"
from = ["Bringing"]
to = "Placed"
params = []
"#;

/// The asked place whose last state is Settled, from which a `Leave` binds
/// who left. `carries` are appended.
fn settles_for(carries: &str) -> String {
    format!(
        "{}\n[[state]]\nname = \"left_by\"\ntype = \"string\"\ninitial = \"\"\n\n\
         [[action]]\nname = \"Leave\"\nkind = \"input\"\nfrom = [\"Settled\"]\nto = \"Unplaced\"\n\
         params = [{{ name = \"left_by\", type = \"string\", source = \"authenticated_subject\" }}]\n\
         {carries}",
        ASKED
            .replace("\"Bringing\", \"Placed\"]", "\"Bringing\", \"Settled\"]")
            .replace("to = \"Placed\"", "to = \"Settled\"")
    )
}

const LEAVE_AS_ASKER: &str =
    "\n[[carry]]\nfrom = \"Placed\"\nto = \"Settled\"\nthen = \"Leave\"\nas = \"asked_by\"\n";

/// An agent acting for `user`, as the platform's MCP route signs one.
fn acting_for(user: &str) -> AgentContext {
    AgentContext {
        security_ctx: Some(SecurityContext::from_verified_jwt(
            "mcp-agent",
            PrincipalKind::Agent,
            Some("claude"),
            Some(user),
            None,
            None,
        )),
        ..Default::default()
    }
}

impl Fixture {
    /// `p-ada` and `p-bo` asked for by those users, and `p-none` brought with
    /// no one asking, each placed under [`ASKED`].
    async fn asked_places(&self) {
        self.install(ASKED).await.expect("first version");
        for (id, user) in [("p-ada", "ada"), ("p-bo", "bo")] {
            self.create(id).await;
            let asked = self
                .state
                .server
                .dispatch_tenant_action(
                    &self.tenant,
                    "Place",
                    id,
                    "Ask",
                    serde_json::json!({}),
                    &acting_for(user),
                )
                .await
                .expect("ask for the place");
            assert!(asked.success, "{:?}", asked.error);
            assert_eq!(asked.state.fields["asked_by"], user);
        }
        self.create("p-none").await;
        let brought = self.act("p-none", "Bring", serde_json::json!({})).await;
        assert_eq!(brought, (true, "Bringing".to_string()));
        for id in ["p-ada", "p-bo", "p-none"] {
            let placed = self.act(id, "Placed", serde_json::json!({})).await;
            assert_eq!(placed, (true, "Placed".to_string()));
        }
    }

    async fn field(&self, id: &str, field: &str) -> serde_json::Value {
        self.state
            .server
            .get_tenant_entity_state(&self.tenant, "Place", id)
            .await
            .unwrap_or_else(|error| panic!("read Place {id}: {error}"))
            .state
            .fields
            .get(field)
            .cloned()
            .unwrap_or_default()
    }
}

/// Each record's `then` move is made for the user it names, so a parameter
/// bound to the caller's identity is bound to theirs. A record that names no
/// one stays where the carry puts it, and the install says so.
#[tokio::test]
async fn a_then_move_is_made_for_the_user_its_record_names() {
    let places = Fixture::new("then-as").await;
    places.asked_places().await;

    let installed = places
        .install(&settles_for(LEAVE_AS_ASKER))
        .await
        .expect("each asker can Leave");
    assert!(
        installed
            .warnings
            .iter()
            .any(|warning| warning.message.contains("`asked_by` is empty")
                && warning.message.contains("(1 now)")),
        "{:?}",
        installed.warnings
    );
    let report = places.carry().await;
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert_eq!(report.carried, 3);
    assert!(
        report.left.len() == 1 && report.left[0].contains("p-none"),
        "{:?}",
        report.left
    );
    for (id, user) in [("p-ada", "ada"), ("p-bo", "bo")] {
        assert_eq!(places.status(id).await, "Unplaced");
        assert_eq!(places.field(id, "left_by").await, user);
    }
    assert_eq!(places.status("p-none").await, "Settled");
    let left_by = places.field("p-none", "left_by").await;
    assert_eq!(
        left_by.as_str().unwrap_or_default(),
        "",
        "no one left p-none"
    );
}

/// The dry run asks the version's Cedar, as the install will serve it, for
/// each user a `then` move is made for: a move it refuses for one of them
/// refuses the install, before anything is written.
#[tokio::test]
async fn a_then_move_cedar_refuses_for_a_user_refuses_the_install() {
    let places = Fixture::new("then-as-cedar").await;
    places.asked_places().await;
    fs::write(
        places.apps_root.join(APP).join("policies/leave.cedar"),
        "forbid(principal, action == Action::\"Leave\", resource) \
         unless { context has actingFor && context.actingFor == \"ada\" };",
    )
    .expect("write the policy");

    let refused = places
        .install(&settles_for(LEAVE_AS_ASKER))
        .await
        .expect_err("only ada may Leave");
    assert!(
        refused.contains("then move Leave is refused by Cedar") && refused.contains("p-bo"),
        "{refused}"
    );
    assert!(!refused.contains("p-ada"), "ada may Leave: {refused}");
    assert_eq!(places.status("p-bo").await, "Placed", "nothing is written");
}

/// `as` names a field only the kernel writes from a caller's identity. A
/// version where anyone else could write it is refused, with the way the
/// field could be chosen.
#[tokio::test]
async fn as_names_a_field_only_a_caller_identity_writes() {
    let places = Fixture::new("then-as-trust").await;
    places.asked_places().await;
    let cases = [
        // A spec that is not strict holds no bound param, so no move binds
        // the field there: only a create body or a field update writes it.
        (
            settles_for(LEAVE_AS_ASKER)
                .replace("strict_action_params = true\n", "")
                .replace(
                    "params = [{ name = \"asked_by\", type = \"string\", source = \"authenticated_subject\" }]",
                    "params = []",
                )
                .replace(", source = \"authenticated_subject\" }", " }"),
            "needs `strict_action_params = true`",
        ),
        (
            settles_for(LEAVE_AS_ASKER).replace(
                "{ name = \"asked_by\", type = \"string\", source = \"authenticated_subject\" }",
                "{ name = \"asked_by\", type = \"string\" }",
            ),
            "action 'Ask' takes 'asked_by' from its caller",
        ),
        (
            settles_for(&LEAVE_AS_ASKER.replace("as = ", "set = { asked_by = \"ada\" }\nas = ")),
            "a [[carry]] sets 'asked_by'",
        ),
    ];
    for (spec, why) in cases {
        let refused = places.install(&spec).await.expect_err(why);
        assert!(refused.contains(why), "{why}: {refused}");
    }
}
