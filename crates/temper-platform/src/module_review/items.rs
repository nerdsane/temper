//! What Jev looks for in a module, in Rita's order (26 Oct 2026). A module may only compute a value between two
//! moves: parse, format, score, transform, or call one outside service and return its answer. Each item is a
//! statement Jev rates 0..1 (`shows`, calibrated on labelled modules: every planted item 0.74 to 0.98, every clean
//! module 0.34 or less), and the words of its finding (`why`, `instead`).

/// One thing a module should not do, and what should do it instead.
#[derive(Debug, Clone, Copy)]
pub struct Item {
    pub number: u8,
    pub kind: &'static str,
    pub title: &'static str,
    pub shows: &'static str,
    pub why: &'static str,
    pub instead: &'static str,
}

/// Raise when a statement or the state's wording changes, so a cached review of the old wording is not reused.
pub const QUESTION_VERSION: u32 = 1;

/// At or above this rating a module is said to show an item: the calibration's gap lies between 0.34 and 0.74.
pub const FOUND_AT: f64 = 0.5;

/// What a clean module is, rated beside the items so it has something true to say about itself.
pub const LEAF: &str = "The module only computes a value between two moves: it parses, formats, scores or transforms data, or makes one outside call and returns its answer.";

pub const ITEMS: [Item; 7] = [
    Item {
        number: 1,
        kind: "chooses_move",
        title: "Choosing the move",
        shows: "The module's answer is a word that tells the record where to go next, such as an action name (\"Approve\", \"Close\"), a status (\"done\", \"waiting\") or a phase, chosen by conditions inside the module.",
        why: "the spec only follows a choice made in code, so the checker cannot see which moves can happen and a person reading the record cannot tell why it moved",
        instead: "return the plain facts the choice rests on (an amount, a yes or no, a count) and give each possible move its own action from this state with a guard on those facts",
    },
    Item {
        number: 2,
        kind: "hidden_states",
        title: "A hidden state machine",
        shows: "The module keeps steps of its own across runs: it reads a phase, step, stage or progress value back from the record's fields or from files, does that step's work, and writes the next one, so it walks through steps that the record's states in the spec do not show.",
        why: "the record's state says less than the truth, so nobody can see or resume where it is, and the checker cannot prove the steps go in order",
        instead: "make each phase a state of the record, with an action between each pair, and let each action's trigger do only that phase's work",
    },
    Item {
        number: 3,
        kind: "waits",
        title: "Waiting or repeating",
        shows: "The module waits or repeats inside one run: it loops over outside calls, sleeps, retries, or polls until something outside is ready.",
        why: "a run that waits holds the module for its whole wait, a restart loses where it was, and the person cannot see that it is waiting",
        instead: "a waiting state with a state_timeout that fires the check again, a counter field for the tries, and a guard that moves the record to Failed after the last one",
    },
    Item {
        number: 4,
        kind: "several_steps",
        title: "Several steps in one",
        shows: "The module does two or more different things in one run: it calls two or more different outside services, or calls one and then uses the answer to decide on a further call. Calling the same service again while waiting for it is not this.",
        why: "a failure halfway leaves the first step done with no state saying so, and a retry repeats it",
        instead: "one trigger per outside call, each from its own state with its own on_success and on_failure, and a guard on the first answer to choose the next step",
    },
    Item {
        number: 5,
        kind: "rules",
        title: "Rules that belong to the spec or Cedar",
        shows: "The module enforces a rule: it decides who may act, refuses amounts or counts over a limit, or checks preconditions before acting.",
        why: "a rule in code is invisible to the checker and to Cedar, so it cannot be proven, and the person is refused with no rule they can read",
        instead: "a guard on the action for limits and preconditions (with an invariant if it must always hold), and a Cedar permit for who may act",
    },
    Item {
        number: 6,
        kind: "hidden_logic",
        title: "Logic the spec can't see",
        shows: "The module makes a decision about the app's own matter, an if or match on its records' values that picks what to do (which service to call, what to send, whether to act), and does not return that decision as a plain value. Checking who may act or a limit is not this; neither is parsing or formatting.",
        why: "the decision shapes what happens, but no field records it, so the spec cannot act on it and a person cannot see or change it",
        instead: "return the decision as a plain field and let guards on the following actions act on it",
    },
    Item {
        number: 7,
        kind: "several_concerns",
        title: "Several concerns in one module",
        shows: "The module does three or more unrelated jobs in one run (for example resizing a photo, counting votes and writing a reminder text), each of which could be its own step of the record.",
        why: "concerns packed together hide the steps between them, so the record cannot show where it is and one failure takes the rest with it",
        instead: "split it into one small module per concern, each run by its own action from its own state, with the record's fields carrying what passes between them",
    },
];

/// Lines that say something past which a module is a finding of its own (item 7), asking why it is not states.
/// Rita, 26 Oct 2026: "I don't know if I can even think of cases where it could be a big module and it still
/// complies." In the census of her apps that day, every module with no finding was under 100 lines and every one over
/// 400 had three findings or more.
pub const LARGE_LINES: usize = 200;
