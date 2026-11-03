//! Seeded simulations of a member's calls while the member is removed and
//! rejoins.
//!
//! Each seed runs, at once and on one thread: Alice's Member record moving
//! Remove and Rejoin (with passivations of its actor between some moves), her
//! own reads of a note, and her agent's `Touch` on it. The cooperative yield
//! points (`buggify_yield`) take a seeded number of turns, so the calls and
//! moves interleave differently on each seed and the same seed replays.
//!
//! The invariant: a call that starts after a move's answer returned, and ends
//! before the next move starts, is decided on that move's status. A call that
//! overlaps a move may go either way.

use std::cell::RefCell;

use axum::http::StatusCode;
use temper_runtime::buggify::{buggify_yield, install_buggify_context};
use temper_runtime::scheduler::install_deterministic_context;

use super::fixture::{
    ALICE, agent_for, app, in_person, move_member, passivate_member, read_note, touch_note,
};

/// Seeds explored on every run.
const EXPLORED_SEEDS: u64 = 24;
/// Moves of Alice's record in one seed.
const MOVES: usize = 6;
/// Calls each caller makes in one seed.
const CALLS: usize = 10;

/// What the mover has done so far, read by the callers.
#[derive(Clone, Debug)]
struct Ledger {
    started: usize,
    answered: usize,
    status: String,
}

/// The workload's own choices, drawn from the seed (xorshift64).
struct Workload(u64);

impl Workload {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self, below: u64) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x % below
    }
}

async fn mover(state: &temper_platform::state::PlatformState, ledger: &RefCell<Ledger>, seed: u64) {
    let mut workload = Workload::new(seed);
    for _ in 0..MOVES {
        buggify_yield(workload.next(6) as u32).await;
        if workload.next(3) == 0 {
            passivate_member(state, ALICE).await;
        }
        let action = if ledger.borrow().status == "Active" {
            "Remove"
        } else {
            "Rejoin"
        };
        ledger.borrow_mut().started += 1;
        let status = move_member(state, ALICE, action).await;
        let mut ledger = ledger.borrow_mut();
        ledger.answered += 1;
        ledger.status = status;
    }
}

/// Make `CALLS` calls of one kind; each settled call's decision against the
/// status it must follow. `Err` is the first violation.
async fn caller(
    state: &temper_platform::state::PlatformState,
    ledger: &RefCell<Ledger>,
    by_agent: bool,
) -> Result<usize, String> {
    let token = if by_agent {
        agent_for(ALICE)
    } else {
        in_person(ALICE)
    };
    let mut settled = 0;
    for call in 0..CALLS {
        buggify_yield(4).await;
        let before = ledger.borrow().clone();
        let answer = if by_agent {
            touch_note(state, &token).await
        } else {
            read_note(state, &token).await
        };
        let after = ledger.borrow().clone();
        if before.started != before.answered || after.started != before.started {
            continue;
        }
        settled += 1;
        let expected = if before.status == "Active" {
            StatusCode::OK
        } else {
            StatusCode::FORBIDDEN
        };
        if answer != expected {
            let who = if by_agent {
                "agent's Touch"
            } else {
                "own read"
            };
            return Err(format!(
                "Alice's {who} {call} answered {answer} after move {} left her {}; expected {expected}",
                before.answered, before.status
            ));
        }
    }
    Ok(settled)
}

#[tokio::test]
async fn dst_the_next_call_follows_the_members_status() {
    let mut settled = 0;
    for seed in 0..EXPLORED_SEEDS {
        let _sim = install_deterministic_context(seed);
        let state = app(seed).await;
        let ledger = RefCell::new(Ledger {
            started: 0,
            answered: 0,
            status: "Active".to_string(),
        });
        let _faults = install_buggify_context(seed);
        let ((), own, agent) = tokio::join!(
            mover(&state, &ledger, seed),
            caller(&state, &ledger, false),
            caller(&state, &ledger, true),
        );
        let own = own.unwrap_or_else(|violation| panic!("seed {seed}: {violation}"));
        let agent = agent.unwrap_or_else(|violation| panic!("seed {seed}: {violation}"));
        settled += own + agent;
    }
    // A harness whose calls all overlapped a move would check nothing.
    assert!(settled > 0, "no call was settled on any seed");
}
