use std::collections::{BTreeMap, BTreeSet};

use super::super::super::room::block;
use super::super::react::Sent;
use super::*;
use crate::model::TemperModelAction;

#[test]
fn move_capacity_counts_against_the_room() {
    let model = JointModel::whole(&BTreeMap::new(), BTreeSet::new());
    let room = 64 << 10;
    model.expansion.allow(room);
    let mut moves = Vec::new();
    for _ in 0..257 {
        if !model.keep_move(
            &mut moves,
            Move::Take(
                "A".into(),
                TemperModelAction {
                    name: "Go".into(),
                    target_state: None,
                    params: BTreeMap::new(),
                },
                0,
            ),
        ) {
            break;
        }
        assert!(
            moves.heap() <= room,
            "{} bytes in a {room}-byte room",
            moves.heap()
        );
    }
}

#[test]
fn move_growth_counts_both_allocations() {
    let model = JointModel::whole(&BTreeMap::new(), BTreeSet::new());
    let mv = Move::Timer("A".into(), "Go".into());
    // The final two-entry vector fits; the old and new blocks together do not.
    let room = block(2 * size_of::<Move>()) + 2 * mv.heap();
    model.expansion.allow(room);
    let mut moves = Vec::new();
    assert!(model.keep_move(&mut moves, mv.clone()));
    let capacity = moves.capacity();
    assert!(!model.keep_move(&mut moves, mv));
    assert!(model.expansion.short());
    assert_eq!(moves.len(), 1);
    assert_eq!(moves.capacity(), capacity, "refuse before allocating");
}

fn calling_model(calls: usize) -> JointModel {
    use super::super::Sending;
    use temper_spec::automaton::parse_automaton;
    let mut source = String::from(
        "[automaton]\nname = 'Ask'\nstates = ['Ready', 'Done']\ninitial = 'Ready'\n\n\
         [[action]]\nname = 'Go'\nkind = 'input'\nfrom = ['Ready']\nto = 'Done'\n",
    );
    for i in 0..calls {
        source.push_str(&format!(
            "\n[[action.triggers]]\nname = 'call{i}'\nkind = 'wasm'\nmodule = '{}{i}'\n",
            "m".repeat(512),
        ));
    }
    let spec = parse_automaton(&source).unwrap();
    JointModel::new(
        &BTreeMap::from([("Ask", &spec)]),
        &BTreeSet::from(["Ask".into()]),
        "Ask",
        &[],
        Sending {
            policies: None,
            reserve: None,
            kernel_moves: true,
            relies_on: BTreeSet::new(),
        },
        false,
    )
}

#[test]
fn module_calls_count_before_inflight_pruning_and_stay_charged() {
    use super::super::super::senders::OUTSIDE;
    let model = calling_model(100);
    let state = model.inits().next().unwrap();
    let action = TemperModelAction {
        name: "Go".into(),
        target_state: Some("Done".into()),
        params: BTreeMap::new(),
    };
    model.expansion.allow(64 << 10);
    model.next(&state, &Move::Take("Ask".into(), action, OUTSIDE));
    assert!(
        model.expansion.short(),
        "pruned calls still consume room while built"
    );

    let room = 1 << 20;
    model.expansion.allow(room);
    let params = Default::default();
    let sent = Sent {
        chain: &[],
        by: OUTSIDE,
        params: &params,
        request: false,
    };
    let outcomes = model.after_commit(state, "Ask", "Go", sent);
    assert!(!model.expansion.short());
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].0.calls.len(), 100);
    assert!(
        !model.expansion.hold(
            room - (block(outcomes.capacity() * size_of::<(JointState, Certainty)>())
                + outcomes[0].0.calls.heap())
                + 1
        ),
        "returned states must remain charged"
    );
}

#[test]
fn duplicate_outcomes_only_reserve_distinct_states() {
    use super::super::super::room::{Bound, Bytes, Progress};
    use temper_spec::automaton::parse_automaton;
    let mut source = String::from(
        "[automaton]\nname = 'Many'\nstates = ['Start', 'Done']\ninitial = 'Start'\n\n\
         [[action]]\nname = 'Go'\nkind = 'input'\nfrom = ['Start', 'Done']\nto = 'Done'\n",
    );
    for i in 0..9 {
        source.push_str(&format!(
            "\n[[action.triggers]]\nname = 'again{i}'\nkind = 'entity'\n\
             target_entity = 'Many'\ntarget_action = 'Go'\n\
             guard = \"Other[Id].status == 'Open'\"\n\
             [action.triggers.resolve_target]\nkind = 'same_id'\n"
        ));
    }
    let many = parse_automaton(&source).unwrap();
    // `Other` is outside the scope, so each trigger's guard is undecided and
    // the commit goes every way: 2^9 outcomes onto a handful of states.
    let other =
        parse_automaton("[automaton]\nname = 'Other'\nstates = ['Open']\ninitial = 'Open'\n")
            .unwrap();
    let model = std::sync::Arc::new(JointModel::whole(
        &BTreeMap::from([("Many", &many), ("Other", &other)]),
        BTreeSet::from(["Many".into()]),
    ));
    let (room, progress) = (Bytes::new(1 << 28), Progress::default());
    let explored = model.walk_with(
        Bound {
            room: &room,
            progress: &progress,
        },
        super::super::Keep::Edges,
        |_, _, _, _| false,
    );
    assert!(explored.stopped.is_none(), "{:?}", explored.stopped);
    assert!(explored.edges.len() >= 512, "exercise duplicate edges");
    assert!(
        explored.nodes.capacity() <= 2 * explored.nodes.len().max(2),
        "{} slots for {} states",
        explored.nodes.capacity(),
        explored.nodes.len()
    );
    drop(explored);
    assert_eq!(room.held(), 0);
}

#[test]
fn table_growth_keeps_previous_allocations_charged() {
    use super::super::super::room::{Bound, Bytes, Progress, grow, grow_map};
    let (room, progress) = (Bytes::new(40), Progress::default());
    let mut held = Held::new(Bound {
        room: &room,
        progress: &progress,
    });
    let mut first: Vec<u64> = Vec::new();
    let mut second: Vec<u64> = Vec::new();
    grow(&mut first, 3, &mut held).unwrap();
    assert!(grow(&mut second, 3, &mut held).is_err());
    assert_eq!(second.capacity(), 0, "refuse before allocating");

    let room = Bytes::new(4096);
    let mut held = Held::new(Bound {
        room: &room,
        progress: &progress,
    });
    let mut map = HashMap::<u32, u32>::new();
    for key in 0..32 {
        grow_map(&mut map, 1, &mut held).unwrap();
        map.insert(key, key);
        assert_eq!(held.bytes, map_bytes::<u32, u32>(map.capacity()));
    }
}
