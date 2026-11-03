//! Measure how fast an order of guided walks finds breaks the breadth-first
//! walk cannot reach (`composite::joint::guided_search`).
//!
//! Each case is a record that walks a chain of `depth` statuses by moves
//! from outside and then falls into Trap, with `flags` booleans anyone may
//! set and clear in any status; its invariant breaks in Trap, and in the
//! "needle" cases only with `needle` of the flags set; a "decoy" is a side
//! chain of that many statuses, entered early, that leads nowhere, so an
//! order that chases new statuses is led away first. The joint states
//! number about (depth + 2) * 2^flags, so a room of a few MB holds only the
//! chain's first steps breadth first.
//!
//! For each case it prints the states the breadth-first walk expands until
//! it meets the break with all the memory it wants, then, in the small room,
//! the expansions (steps) and distinct states phase two needed under each
//! order: `plain` (what installs use) and `uniform` (every step at random),
//! the baseline any other order has to beat.
//!
//! Run: `cargo run --release -p temper-verify --example guided_orders [room MB]`.

use temper_spec::automaton::{Automaton, parse_automaton};
use temper_verify::composite::joint::{Candidate, GUIDED_STEPS, guided_search, plain};

fn deep(depth: usize, flags: usize, needle: usize, decoy: usize) -> String {
    let mut statuses: Vec<String> = (0..=depth).map(|j| format!("S{j}")).collect();
    statuses.push("Trap".into());
    statuses.extend((0..decoy).map(|j| format!("X{j}")));
    let all = statuses
        .iter()
        .map(|s| format!("'{s}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut spec = format!("[automaton]\nname = 'Deep'\nstates = [{all}]\ninitial = 'S0'\n");
    for i in 0..flags {
        spec.push_str(&format!(
            "\n[[state]]\nname = 'b{i}'\ntype = 'bool'\ninitial = false\n"
        ));
    }
    for j in 0..depth {
        spec.push_str(&format!(
            "\n[[action]]\nname = 'Step{j}'\nkind = 'input'\nfrom = ['S{j}']\nto = 'S{}'\n",
            j + 1
        ));
    }
    spec.push_str(&format!(
        "\n[[action]]\nname = 'Fall'\nkind = 'input'\nfrom = ['S{depth}']\nto = 'Trap'\n"
    ));
    // A side chain of statuses that leads nowhere, entered from S1.
    for j in 0..decoy {
        let from = if j == 0 {
            "S1".to_string()
        } else {
            format!("X{}", j - 1)
        };
        spec.push_str(&format!(
            "\n[[action]]\nname = 'Side{j}'\nkind = 'input'\nfrom = ['{from}']\nto = 'X{j}'\n"
        ));
    }
    for i in 0..flags {
        spec.push_str(&format!(
            "\n[[action]]\nname = 'Set{i}'\nkind = 'input'\nfrom = [{all}]\neffect = ['b{i} = true']\n\
             \n[[action]]\nname = 'Clear{i}'\nkind = 'input'\nfrom = [{all}]\neffect = ['b{i} = false']\n"
        ));
    }
    let read = (0..flags)
        .map(|i| format!("(b{i} || !b{i})"))
        .collect::<Vec<_>>()
        .join(" && ");
    let premise = std::iter::once("status == 'Trap'".to_string())
        .chain((0..needle).map(|i| format!("b{i}")))
        .collect::<Vec<_>>()
        .join(" && ");
    spec.push_str(&format!(
        "\n[[invariant]]\nname = 'rule'\nassert = \"{premise} => Other[Id].status == 'Closed' && {read}\"\n"
    ));
    spec
}

const OTHER: &str = "[automaton]\nname = 'Other'\nstates = ['Open', 'Closed']\ninitial = 'Open'\n";

fn main() {
    let room_mb: usize = std::env::args()
        .nth(1)
        .map(|arg| arg.parse().expect("room in MB"))
        .unwrap_or(2);
    let cases = [
        (8, 10, 0, 0),
        (16, 10, 0, 0),
        (24, 10, 0, 0),
        (32, 10, 0, 0),
        (48, 10, 0, 0),
        (24, 12, 0, 0),
        (24, 8, 0, 0),
        (16, 10, 2, 0),
        (16, 10, 4, 0),
        (24, 10, 3, 0),
        (24, 12, 6, 0),
        (24, 10, 0, 24),
        (24, 10, 0, 96),
        (24, 10, 3, 96),
    ];
    println!(
        "room {room_mb} MB, {GUIDED_STEPS} steps; columns: depth flags needle decoy | breadth-first states to the break (unbounded room) | plain: steps distinct | uniform: steps distinct"
    );
    for (depth, flags, needle, decoy) in cases {
        let sources = [deep(depth, flags, needle, decoy), OTHER.to_string()];
        let automatons: Vec<Automaton> = sources
            .iter()
            .map(|s| parse_automaton(s).expect("case parses"))
            .collect();
        let refs: Vec<&Automaton> = automatons.iter().collect();
        let unbounded =
            guided_search(&refs, "Deep", 8 << 30, GUIDED_STEPS, &mut plain).expect("case checks");
        let mut row = format!(
            "{depth:>3} {flags:>3} {needle:>3} {decoy:>3} | {:>9}",
            if unbounded.bfs_found {
                unbounded.bfs_states.to_string()
            } else {
                "-".into()
            }
        );
        let mut uniform = |_: &Candidate<'_>| 0;
        for order in [
            &mut plain as &mut dyn FnMut(&Candidate<'_>) -> u32,
            &mut uniform,
        ] {
            let found = guided_search(&refs, "Deep", room_mb << 20, GUIDED_STEPS, order)
                .expect("case checks");
            let cell = match (found.bfs_found, found.found) {
                (true, _) => format!("bfs {}", found.bfs_states),
                (false, Some((steps, distinct))) => format!("{steps:>6} {distinct:>7}"),
                (false, None) => format!("  none {:>7}", found.distinct),
            };
            row.push_str(&format!(" | {cell:>14}"));
        }
        println!("{row}");
    }
}
