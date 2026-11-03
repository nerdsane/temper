//! A chain names whoever started it, so a lost row's timer fires as them.

use super::*;

/// A record made and moved only by a chain, and who the chain says it ran as.
struct Shipped {
    /// Who the shipment's newest move says it ran as, before the restart.
    moved_by: Value,
    /// Who its timer fired as after losing its row.
    fired_by: Value,
}

/// `by`'s request sets off a reaction whose effect makes the Shipment, its timer
/// row is lost while the server is down, and the server restarts. `owner`, when
/// given, moved the Line first.
fn ship_and_lose_the_row(sim: &mut Sim, owner: Option<AgentContext>, by: AgentContext) -> Shipped {
    let merge_at = sim.draw(500);
    let first = sim.start(3600);
    let (shipment, moved_by) = first.run(async {
        let state = &first.state;
        wait(merge_at).await;
        if let Some(owner) = owner {
            act(state, "Line", "line", "Tidy", &owner).await;
        }
        act(state, "Line", "line", "Merge", &by).await;
        let line = until(state, "Lines", "line", "shipped", |line| {
            line["fields"]["rollout"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
        })
        .await;
        let shipment = line["fields"]["rollout"].as_str().unwrap().to_string();
        let record = until(state, "Shipments", &shipment, "started", |record| {
            record["status"] == "Staging"
        })
        .await;
        (shipment, record["last_event_by"].clone())
    });
    sim.stop(first);
    let rows = sim.store.list_state_timers("default");
    let row = rows
        .iter()
        .find(|row| row.entity_id == shipment)
        .expect("the shipment's timer left a row");
    sim.store
        .delete_state_timer("default", "Shipment", &shipment, &row.arm_id);

    let after = sim.start(1);
    let record = after.run(until(
        &after.state,
        "Shipments",
        &shipment,
        "fired",
        |record| record["status"] == "Diagnosed",
    ));
    Shipped {
        moved_by,
        fired_by: record["fields"]["fired_by"].clone(),
    }
}

#[test]
fn a_chain_a_person_started_keeps_a_lost_rows_timer_as_that_person() {
    Sim::each_seed(|sim| {
        let shipped = ship_and_lose_the_row(sim, None, person("ann"));
        assert_eq!(shipped.moved_by, "ann", "seed {}", sim.seed);
        assert_eq!(shipped.fired_by, "ann", "seed {}", sim.seed);
    });
}

#[test]
fn a_chain_an_agent_started_keeps_a_lost_rows_timer_as_that_agent() {
    Sim::each_seed(|sim| {
        let shipped = ship_and_lose_the_row(sim, None, agent_for("bea"));
        assert_eq!(shipped.moved_by, "agent-1", "seed {}", sim.seed);
        assert_eq!(
            shipped.fired_by, "bea",
            "seed {}: the action binds whom the agent acts for",
            sim.seed
        );
    });
}

/// A guest's chain on a record its owner moved first names the guest, never
/// the owner.
#[test]
fn a_chain_a_non_owner_started_names_them_not_the_owner() {
    Sim::each_seed(|sim| {
        let shipped = ship_and_lose_the_row(sim, Some(person("owner")), person("guest"));
        assert_eq!(shipped.moved_by, "guest", "seed {}", sim.seed);
        assert_eq!(shipped.fired_by, "guest", "seed {}", sim.seed);
    });
}
