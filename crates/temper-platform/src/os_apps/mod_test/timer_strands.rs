//! A state timer's fire can be lost: a projection timeout, a restart before
//! the boot re-arm, a refused fire. When no one outside Temper may make the
//! timer's move, or a move in its `reset_on`, nothing redoes a lost fire and
//! the record stays in its state. The install of any app says so
//! (`timer_strands`) and installs it all the same.

use super::*;

/// Start puts a job in Making, and Making's timer fails it after 4500 s.
/// Nudge leaves it in Making.
const JOB: &str = r#"
[automaton]
name = "Job"
states = ["Queued", "Making", "Failed"]
initial = "Queued"

[[action]]
name = "Start"
kind = "input"
from = ["Queued"]
to = "Making"

[[action]]
name = "Nudge"
kind = "input"
from = ["Making"]

[[action]]
name = "WorkFailed"
from = ["Making"]
to = "Failed"

[[state_timeout]]
state = "Making"
after_seconds = 4500
on_timeout = "WorkFailed"
"#;

/// Open's timer closes a batch after a day, and so may a member.
const BATCH: &str = r#"
[automaton]
name = "Batch"
states = ["Open", "Closed"]
initial = "Open"

[[action]]
name = "Close"
kind = "input"
from = ["Open"]
to = "Closed"

[[state_timeout]]
state = "Open"
after_seconds = 86400
on_timeout = "Close"
"#;

const CSDL: &str = r#"<?xml version="1.0"?><edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx"><edmx:DataServices><Schema Namespace="Timers" xmlns="http://docs.oasis-open.org/odata/ns/edm"><EntityType Name="Job"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/></EntityType><EntityType Name="Batch"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/></EntityType><EntityContainer Name="Container"><EntitySet Name="Jobs" EntityType="Timers.Job"/><EntitySet Name="Batches" EntityType="Timers.Batch"/></EntityContainer></Schema></edmx:DataServices></edmx:Edmx>"#;

/// A member makes every move but WorkFailed, which only Making's timer makes.
const MEMBER_MAKES_ALL_BUT_WORK_FAILED: &str = r#"
permit(principal, action in [Action::"create", Action::"read", Action::"list", Action::"Start", Action::"Nudge", Action::"Close"], resource)
when { principal has role && principal.role == "member" };
"#;

/// Install an app of `job` and the batch, with `cedar` as its policies, into
/// a tenant of its own.
async fn install(label: &str, job: &str, cedar: &str) -> InstallResult {
    let id = uuid::Uuid::new_v4().simple();
    let app = format!("timer-strands-{label}-{id}");
    let apps_root = std::env::temp_dir().join(format!("temper-{app}"));
    let dir = apps_root.join(&app);
    fs::create_dir_all(dir.join("specs")).expect("create specs dir");
    fs::create_dir_all(dir.join("policies")).expect("create policies dir");
    fs::write(
        dir.join("app.toml"),
        format!("name = \"{app}\"\ndescription = \"Timers\"\nversion = \"0.1.0\"\n"),
    )
    .expect("write app.toml");
    fs::write(dir.join("APP.md"), "# Timers\n").expect("write APP.md");
    fs::write(dir.join("specs/model.csdl.xml"), CSDL).expect("write CSDL");
    fs::write(dir.join("specs/job.ioa.toml"), job).expect("write the job spec");
    fs::write(dir.join("specs/batch.ioa.toml"), BATCH).expect("write the batch spec");
    fs::write(dir.join("policies/members.cedar"), cedar).expect("write policy");
    let state = PlatformState::new(None);
    // Other tests reload the global catalog; register right before use.
    add_os_apps_dir(apps_root);
    install_os_app(&state, &format!("timers-{label}-{id}"), &app)
        .await
        .expect("a timer warning never refuses an install")
}

fn strands(installed: &InstallResult) -> Vec<&InstallWarning> {
    installed
        .warnings
        .iter()
        .filter(|warning| warning.check == "timer_strands")
        .collect()
}

#[tokio::test]
async fn the_install_names_a_timer_no_one_outside_may_redo() {
    let installed = install("stranded", JOB, MEMBER_MAKES_ALL_BUT_WORK_FAILED).await;
    assert_eq!(installed.added, ["Batch", "Job"], "the install goes ahead");
    let found = strands(&installed);
    let [strand] = found.as_slice() else {
        panic!("one timer strands, Job's: {:?}", installed.warnings);
    };
    assert!(
        ["Job", "Making", "WorkFailed", "4500 s"]
            .iter()
            .all(|named| strand.message.contains(named)),
        "{}",
        strand.message
    );
    assert_eq!(
        strand.detail,
        Some(serde_json::json!({
            "entity_type": "Job",
            "state": "Making",
            "on_timeout": "WorkFailed",
            "after_seconds": 4500,
        }))
    );
}

#[tokio::test]
async fn a_timer_a_member_may_redo_or_arm_again_is_not_named() {
    let member_fails_work = MEMBER_MAKES_ALL_BUT_WORK_FAILED.replace(
        "Action::\"Close\"]",
        "Action::\"Close\", Action::\"WorkFailed\"]",
    );
    let installed = install("redone", JOB, &member_fails_work).await;
    assert!(strands(&installed).is_empty(), "{:?}", installed.warnings);

    // Nudge is a member's, and in reset_on it arms the timer again.
    let rearmed = format!("{JOB}reset_on = [\"Nudge\"]\n");
    let installed = install("rearmed", &rearmed, MEMBER_MAKES_ALL_BUT_WORK_FAILED).await;
    assert!(strands(&installed).is_empty(), "{:?}", installed.warnings);
}
