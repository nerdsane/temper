//! A new version of a spec may change anything, so long as it leaves no
//! stored record with no way forward: in a state it drops, or in a state
//! where no move is possible for the record any more. A `[[carry]]` says where
//! such records go; the install tries each carry before writing anything, and
//! once it has answered a pass in the background makes it. The records are
//! the type's real stored ones.

use super::*;
use std::path::PathBuf;

use temper_store_turso::TursoEventStore;

mod acts_for;
mod carrying;
mod replays;
mod then;

/// A place an app's Temper is brought to, after `KatagamiAppPlace` before
/// katagami #1130.
const PLACE: &str = r#"
[automaton]
name = "Place"
states = ["Unplaced", "Bringing", "Placed"]
initial = "Unplaced"
allow_indefinite_states = ["Unplaced", "Bringing", "Placed"]

[[state]]
name = "commit"
type = "string"
initial = ""

[[action]]
name = "Bring"
kind = "input"
from = ["Unplaced", "Bringing", "Placed"]
to = "Bringing"
params = [{ name = "commit", type = "string" }]

[[action.constraints]]
kind = "param_nonempty"
param = "commit"

[[action]]
name = "Placed"
kind = "input"
from = ["Bringing"]
to = "Placed"
"#;

/// A record a place may sit under, which the place's rules can read.
const PARENT: &str = r#"
[automaton]
name = "Parent"
states = ["Open", "Closed"]
initial = "Open"
allow_indefinite_states = ["Open", "Closed"]

[[action]]
name = "Close"
kind = "input"
from = ["Open"]
to = "Closed"
"#;

const CSDL: &str = r#"<?xml version="1.0"?><edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx"><edmx:DataServices><Schema Namespace="Places" xmlns="http://docs.oasis-open.org/odata/ns/edm"><EntityType Name="Place"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/></EntityType><EntityType Name="Parent"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/></EntityType><EntityContainer Name="Container"><EntitySet Name="Places" EntityType="Places.Place"/><EntitySet Name="Parents" EntityType="Places.Parent"/></EntityContainer></Schema></edmx:DataServices></edmx:Edmx>"#;

/// The place whose last state is called Settled, not Placed.
fn settles() -> String {
    PLACE
        .replace("\"Bringing\", \"Placed\"]", "\"Bringing\", \"Settled\"]")
        .replace("to = \"Placed\"", "to = \"Settled\"")
}

/// Settles, with a carry that moves Placed records to Settled.
fn settles_carried() -> String {
    format!(
        "{}\n[[carry]]\nfrom = \"Placed\"\nto = \"Settled\"\n",
        settles()
    )
}

/// Settled, for a place brought at least once, counted.
fn settles_once_brought() -> String {
    settles()
        .replace(
            "[[action]]\nname = \"Bring\"",
            "[[state]]\nname = \"brought\"\ntype = \"counter\"\ninitial = 0\n\n\
             [[invariant]]\nname = \"SettledWasBrought\"\nassert = \"status == 'Settled' => brought > 0\"\n\n\
             [[action]]\nname = \"Bring\"",
        )
        .replace(
            "to = \"Bringing\"\n",
            "to = \"Bringing\"\neffect = [\"brought += 1\"]\n",
        )
}

/// The place with a flag set before it may be brought: `ready`, false for a
/// place stored before it. With `ready_move`, a `Ready` move sets it.
fn needs_ready(ready_move: bool) -> String {
    let ready = if ready_move {
        "[[action]]\nname = \"Ready\"\nkind = \"input\"\nfrom = [\"Unplaced\", \"Placed\"]\neffect = [\"ready = true\"]\n\n"
    } else {
        ""
    };
    PLACE
        .replace(
            "[[action]]\nname = \"Bring\"",
            &format!(
                "[[state]]\nname = \"ready\"\ntype = \"bool\"\ninitial = false\n\n{ready}\
                 [[action]]\nname = \"Bring\""
            ),
        )
        .replace(
            "to = \"Bringing\"\n",
            "to = \"Bringing\"\nguard = \"ready\"\n",
        )
}

/// Settles, under a parent: Settled places may only Leave, and only while
/// their parent is open. `carries` are appended.
fn leaves_while_open(carries: &str) -> String {
    format!(
        "{}\n[[state]]\nname = \"parent_id\"\ntype = \"string\"\ninitial = \"\"\n\n\
         [[action]]\nname = \"Leave\"\nkind = \"input\"\nfrom = [\"Settled\"]\n\
         to = \"Unplaced\"\nguard = \"Parent[parent_id].status == 'Open'\"\n{carries}",
        settles().replace(
            "from = [\"Unplaced\", \"Bringing\", \"Settled\"]",
            "from = [\"Unplaced\", \"Bringing\"]",
        )
    )
}

/// Leaves while open, and each Leave closes the parent, through a trigger.
fn leaves_and_closes(carries: &str) -> String {
    leaves_while_open(&format!(
        "\n[[action.triggers]]\nname = \"close-parent\"\nkind = \"entity\"\n\
         target_entity = \"Parent\"\ntarget_action = \"Close\"\n\
         [action.triggers.resolve_target]\nkind = \"field\"\nid_field = \"parent_id\"\n{carries}"
    ))
}

/// katagami #1130's change: each Bring carries a count higher than the last.
fn counts_brings() -> String {
    PLACE
        .replace(
            "[[action]]\nname = \"Bring\"",
            "[[state]]\nname = \"brought\"\ntype = \"counter\"\ninitial = 0\n\n[[action]]\nname = \"Bring\"",
        )
        .replace(
            "params = [{ name = \"commit\", type = \"string\" }]",
            "params = [{ name = \"commit\", type = \"string\" }, { name = \"brought\", type = \"counter\" }]\n\
             effect = [\"brought = params.brought\"]\n\n\
             [[action.constraints]]\nkind = \"param_greater_than_field\"\nparam = \"brought\"\nfield = \"brought\"",
        )
}

struct Fixture {
    state: PlatformState,
    tenant: TenantId,
    root: PathBuf,
    apps_root: PathBuf,
}

const APP: &str = "spec-change-places";

impl Fixture {
    async fn new(label: &str) -> Self {
        let id = uuid::Uuid::new_v4();
        let root = std::env::temp_dir().join(format!("temper-spec-change-{label}-{id}"));
        fs::create_dir_all(&root).expect("create the test root");
        Self {
            state: Self::open(&root).await,
            tenant: TenantId::new(format!("spec-change-{label}")),
            apps_root: root.join("apps"),
            root,
        }
    }

    /// A server over the store under `root`, with nothing in memory.
    async fn open(root: &std::path::Path) -> PlatformState {
        let store =
            TursoEventStore::new(&format!("file:{}", root.join("events.db").display()), None)
                .await
                .expect("open the store");
        let mut state = PlatformState::new(None);
        state.server.data_dir = root.join("data");
        state
            .server
            .set_storage_stack(temper_server::StorageStack::from_turso(store));
        state
    }

    /// The same records in a new process: this one stopped.
    async fn restarted(self) -> Self {
        drop(self.state);
        Self {
            state: Self::open(&self.root).await,
            ..self
        }
    }

    /// The carry pass an install or a start runs in the background, awaited.
    async fn carry(&self) -> temper_server::state::CarryReport {
        self.state.server.carry_stranded(&self.tenant).await
    }

    /// Wait for `id` to reach `status`, as a background pass moves it.
    async fn status_becomes(&self, id: &str, status: &str) {
        for _ in 0..250 {
            if self.status(id).await == status {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("{id} is {} after 5 s, not {status}", self.status(id).await);
    }

    /// Install the app with `spec` as its Place.
    async fn install(&self, spec: &str) -> Result<InstallResult, String> {
        let dir = self.apps_root.join(APP);
        fs::create_dir_all(dir.join("specs")).expect("create specs dir");
        fs::create_dir_all(dir.join("policies")).expect("create policies dir");
        fs::write(
            dir.join("app.toml"),
            format!("name = \"{APP}\"\ndescription = \"Places\"\nversion = \"0.1.0\"\n"),
        )
        .expect("write app.toml");
        fs::write(dir.join("APP.md"), "# Places\n").expect("write APP.md");
        fs::write(
            dir.join("policies/allow.cedar"),
            "permit(principal, action, resource);",
        )
        .expect("write policy");
        fs::write(dir.join("specs/model.csdl.xml"), CSDL).expect("write CSDL");
        fs::write(dir.join("specs/place.ioa.toml"), spec).expect("write spec");
        fs::write(dir.join("specs/parent.ioa.toml"), PARENT).expect("write parent spec");
        // Other tests reload the global catalog; register right before use.
        add_os_apps_dir(self.apps_root.clone());
        install_os_app(&self.state, self.tenant.as_str(), APP).await
    }

    async fn act(&self, id: &str, action: &str, params: serde_json::Value) -> (bool, String) {
        let response = self
            .state
            .server
            .dispatch_tenant_action(
                &self.tenant,
                "Place",
                id,
                action,
                params,
                &AgentContext::system(),
            )
            .await
            .unwrap_or_else(|error| panic!("dispatch {action} on {id}: {error}"));
        (response.success, response.state.status)
    }

    async fn status(&self, id: &str) -> String {
        self.status_of("Place", id).await
    }

    async fn status_of(&self, entity_type: &str, id: &str) -> String {
        self.state
            .server
            .get_tenant_entity_state(&self.tenant, entity_type, id)
            .await
            .unwrap_or_else(|error| panic!("read {entity_type} {id}: {error}"))
            .state
            .status
    }

    async fn create(&self, id: &str) {
        self.create_with(id, serde_json::json!({})).await;
    }

    async fn create_with(&self, id: &str, fields: serde_json::Value) {
        self.state
            .server
            .get_or_create_tenant_entity(&self.tenant, "Place", id, fields)
            .await
            .expect("create the place");
    }

    /// Two parents, `open` and `closed`, the second closed.
    async fn open_and_closed_parents(&self) {
        for parent in ["open", "closed"] {
            self.state
                .server
                .get_or_create_tenant_entity(&self.tenant, "Parent", parent, serde_json::json!({}))
                .await
                .expect("create the parent");
        }
        let closed = self
            .state
            .server
            .dispatch_tenant_action(
                &self.tenant,
                "Parent",
                "closed",
                "Close",
                serde_json::json!({}),
                &AgentContext::system(),
            )
            .await
            .expect("close the parent");
        assert!(closed.success, "{:?}", closed.error);
    }

    /// A place stored under the first version, brought to a commit and placed.
    async fn placed(&self, id: &str) {
        self.placed_with(id, serde_json::json!({})).await;
    }

    /// A place created with `fields`, brought to a commit and placed.
    async fn placed_with(&self, id: &str, fields: serde_json::Value) {
        self.create_with(id, fields).await;
        let brought = self
            .act(id, "Bring", serde_json::json!({ "commit": "c1" }))
            .await;
        assert_eq!(brought, (true, "Bringing".to_string()));
        let placed = self.act(id, "Placed", serde_json::json!({})).await;
        assert_eq!(placed, (true, "Placed".to_string()));
    }
}

/// The check tries one record for each group of records that hold the same
/// values of what it reads, and a value only a new invariant reads still
/// splits a group: carried to Settled, the big place breaks the rule and the
/// small one does not.
#[tokio::test]
async fn records_that_differ_only_in_what_an_invariant_reads_are_tried_apart() {
    let places = Fixture::new("grouped").await;
    places.install(PLACE).await.expect("first version");
    places
        .placed_with("p1", serde_json::json!({ "size": "small" }))
        .await;
    places
        .placed_with("p2", serde_json::json!({ "size": "big" }))
        .await;

    let refused = places
        .install(&format!(
            "{}\n[[state]]\nname = \"size\"\ntype = \"string\"\ninitial = \"\"\n\n\
             [[invariant]]\nname = \"OnlySmallSettles\"\n\
             assert = \"status == 'Settled' => size != 'big'\"\n\n\
             [[carry]]\nfrom = \"Placed\"\nto = \"Settled\"\n",
            settles()
        ))
        .await
        .expect_err("p2 is big, so carried to Settled it would break the rule");
    assert!(
        refused.contains("OnlySmallSettles") && refused.contains("p2"),
        "{refused}"
    );
    assert!(!refused.contains("p1"), "p1 is small: {refused}");
}

/// katagami #1130's shape: a new counter, and a constraint on an existing
/// action that reads it. A missing declared counter reads as its initial
/// value (temper #167), so the stored place can still Bring.
#[tokio::test]
async fn a_new_counter_with_a_constraint_is_installed_and_stored_records_still_move() {
    let places = Fixture::new("counter").await;
    places.install(PLACE).await.expect("first version");
    places.placed("p1").await;

    places
        .install(&counts_brings())
        .await
        .expect("the counter strands nothing");
    let brought = places
        .act(
            "p1",
            "Bring",
            serde_json::json!({ "commit": "c2", "brought": 1 }),
        )
        .await;
    assert_eq!(brought, (true, "Bringing".to_string()));
}

/// Count-based: records the installed version already leaves with no way
/// forward are reported, and do not stop a change that strands no others.
#[tokio::test]
async fn records_already_stuck_are_reported_and_do_not_stop_an_install() {
    let places = Fixture::new("already").await;
    places
        .install(&needs_ready(false))
        .await
        .expect("first version");
    places.create("p1").await;
    assert_eq!(
        places
            .act("p1", "Bring", serde_json::json!({ "commit": "c1" }))
            .await,
        (false, "Unplaced".to_string()),
        "p1 is not ready, and nothing makes it so"
    );

    let installed = places
        .install(&needs_ready(false).replace(
            "from = [\"Unplaced\", \"Bringing\", \"Placed\"]",
            "from = [\"Unplaced\", \"Placed\"]",
        ))
        .await
        .expect("p1 had no way forward before this version either");
    assert!(
        installed
            .warnings
            .iter()
            .any(|warning| warning.message.contains("p1")),
        "{:?}",
        installed.warnings
    );
}

#[tokio::test]
async fn callers_cannot_make_a_carry_move() {
    let places = Fixture::new("caller").await;
    places
        .install(&format!(
            "{}\n[[carry]]\nfrom = \"Placed\"\nset = {{ ready = true }}\n",
            needs_ready(false)
        ))
        .await
        .expect("first version");
    places.create("p1").await;
    let refused = places
        .state
        .server
        .dispatch_tenant_action(
            &places.tenant,
            "Place",
            "p1",
            "carry:Placed:1",
            serde_json::json!({ "ready": true }),
            &AgentContext::system(),
        )
        .await;
    match refused {
        Err(error) => assert!(error.contains("[[carry]]"), "{error}"),
        Ok(response) => panic!("a caller made a carry move: {response:?}"),
    }
}
