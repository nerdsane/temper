//! Labels follow reactions, spawns, timers and recovery.

use std::future::Future;
use temper_platform::state::PlatformState;
use temper_store_sim::SimEventStore;

use super::fixture::{
    act, agent, event_names_the_agent, history, record_names_the_agent, server, until_status,
};

/// The label stays down a chain of reactions and onto a record the chain makes.
#[tokio::test(flavor = "multi_thread")]
async fn a_reaction_keeps_the_label_down_the_chain() {
    let state = server(&SimEventStore::no_faults(2), 3600).await;
    act(&state, "j", "Go", &agent()).await;

    let record = until_status(&state, "Jobs", "j", "Done").await;
    assert_eq!(record["last_event_via"], "reaction", "{record}");
    record_names_the_agent(&record);
    let moves = history(&state, "Job", "j").await;
    for action in ["Go", "Hop", "Finish"] {
        let event = moves
            .iter()
            .find(|event| event["action"] == action)
            .unwrap_or_else(|| panic!("{action} is in history: {moves:?}"));
        event_names_the_agent(event);
    }

    let child_id = record["fields"]["child"]
        .as_str()
        .expect("the spawn names its child");
    let child = until_status(&state, "Children", child_id, "Started").await;
    record_names_the_agent(&child);
    event_names_the_agent(history(&state, "Child", child_id).await.last().unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_timer_keeps_the_label_of_the_agent_that_set_it() {
    let state = server(&SimEventStore::no_faults(3), 1).await;
    act(&state, "j", "Arm", &agent()).await;

    let record = until_status(&state, "Jobs", "j", "Expired").await;
    assert_eq!(record["last_event_via"], "timer", "{record}");
    record_names_the_agent(&record);
    event_names_the_agent(history(&state, "Job", "j").await.last().unwrap());
}

/// Run `first` on a server of its own, stopped afterward as a process exit
/// would stop it, timers and all.
fn on_a_server_that_stops<F, Fut>(store: &SimEventStore, after: u32, first: F)
where
    F: FnOnce(PlatformState) -> Fut + Send + 'static,
    Fut: Future<Output = ()>,
{
    let store = store.clone();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async move {
            let state = server(&store, after).await;
            first(state).await;
        });
        drop(runtime);
    })
    .join()
    .expect("the first server ran");
}

/// The timer row says who set it, so a restart fires as them.
#[tokio::test(flavor = "multi_thread")]
async fn a_timer_row_keeps_the_label_across_a_restart() {
    let store = SimEventStore::no_faults(4);
    on_a_server_that_stops(&store, 5, |state| async move {
        act(&state, "j", "Arm", &agent()).await;
    });
    assert!(
        store
            .list_state_timers("default")
            .iter()
            .any(|row| row.entity_id == "j"),
        "the timer left a row"
    );

    let state = server(&store, 5).await;
    let record = until_status(&state, "Jobs", "j", "Expired").await;
    assert_eq!(record["last_event_via"], "timer", "{record}");
    record_names_the_agent(&record);
}

/// With the row gone, the journal says who moved the record, label and all.
#[tokio::test(flavor = "multi_thread")]
async fn a_lost_timer_row_gets_the_label_from_the_journal() {
    let store = SimEventStore::no_faults(5);
    on_a_server_that_stops(&store, 3600, |state| async move {
        act(&state, "j", "Arm", &agent()).await;
    });
    let rows = store.list_state_timers("default");
    let row = rows
        .iter()
        .find(|row| row.entity_id == "j")
        .expect("the timer left a row");
    store.delete_state_timer("default", "Job", "j", &row.arm_id);

    let state = server(&store, 1).await;
    let record = until_status(&state, "Jobs", "j", "Expired").await;
    assert_eq!(record["last_event_via"], "timer", "{record}");
    record_names_the_agent(&record);
    event_names_the_agent(history(&state, "Job", "j").await.last().unwrap());
}
