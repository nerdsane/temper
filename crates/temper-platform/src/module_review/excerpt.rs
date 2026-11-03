//! The parts of an app's specs one module's review reads: the record types whose actions run the module, and of each
//! the actions that run it, the actions its answers make (`on_success`, `on_failure`), the actions out of the states
//! those come from and reach, and the fields they use. Comments are removed, as from the module, so a comment cannot
//! argue the answer; a line is cut as a module's is.
//!
//! What does not fit [`SPEC_BYTES`] is left out farthest first (the actions around, then fields, then the answers'
//! actions) and named, so a review that read part of what it needs says so and never reads as clean.

use std::collections::BTreeSet;

use super::source::cut;
use super::types::SpecSource;

/// Spec text one ask carries, in bytes. The largest excerpt seen is 53 KB: the Factory's `pipeline` module, which 46
/// of its type's 112 actions run (Rita's apps, 26 Oct 2026; the spec is 122 KB, 55 KB without its comments). The next
/// largest are 16 KB and below.
pub const SPEC_BYTES: usize = 64 * 1024;

/// What a module's review reads of the specs, and what it had to leave out.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Excerpt {
    /// What the excerpt is, said to Jev before it.
    pub heading: String,
    pub text: String,
    /// `Order's action Ship`, for each part left out past [`SPEC_BYTES`].
    pub left_out: Vec<String>,
}

/// One top-level table of a spec (`[automaton]`, one `[[action]]` with its triggers, one `[[state]]`), as written.
struct Block<'a> {
    kind: &'a str,
    lines: Vec<&'a str>,
}

/// A block offered to the excerpt: how near the module it is (0 nearest), and how it is named when left out.
struct Piece {
    tier: u8,
    spec: usize,
    block: usize,
    name: String,
}

const ABOUT: &str = "From the app's spec, the record types that run this module (comments removed): the actions that \
run it, the actions its answers make, the actions out of the states those reach, and the fields they use. Other \
actions are left out.";

const WHOLE: &str = "The app's spec (no action runs this module by name; comments removed):";

/// The excerpt of `specs` a review of `module` reads.
pub fn excerpt(module: &str, specs: &[SpecSource]) -> Excerpt {
    let named = specs
        .iter()
        .any(|spec| spec.module_triggers.iter().any(|(_, runs)| runs == module));
    let blocks: Vec<Vec<Block>> = specs.iter().map(|spec| blocks_of(&spec.source)).collect();
    let mut pieces = Vec::new();
    for (index, spec) in specs.iter().enumerate() {
        let near = if named {
            near_module(module, spec, &blocks[index])
        } else {
            Some(whole(spec, &blocks[index]))
        };
        let Some(near) = near else { continue };
        pieces.extend(near.into_iter().map(|(tier, block, name)| Piece {
            tier,
            spec: index,
            block,
            name,
        }));
    }
    pieces.sort_by_key(|piece| (piece.tier, piece.spec, piece.block));

    let mut kept: BTreeSet<(usize, usize)> = BTreeSet::new();
    let mut left_out = Vec::new();
    let mut room = SPEC_BYTES;
    for piece in &pieces {
        let size = text_of(&blocks[piece.spec][piece.block]).len() + 1;
        if size == 1 {
            continue;
        }
        if size <= room {
            room -= size;
            kept.insert((piece.spec, piece.block));
        } else {
            left_out.push(piece.name.clone());
        }
    }
    let text = specs
        .iter()
        .enumerate()
        .map(|(index, _)| {
            blocks[index]
                .iter()
                .enumerate()
                .filter(|(block, _)| kept.contains(&(index, *block)))
                .map(|(_, block)| text_of(block))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    debug_assert!(text.len() <= SPEC_BYTES);
    Excerpt {
        heading: if named { ABOUT } else { WHOLE }.to_string(),
        text,
        left_out,
    }
}

/// `left_out` as a few words: the first five names and how many more.
pub fn said_left_out(left_out: &[String]) -> String {
    const NAMED: usize = 5;
    let mut said = left_out
        .iter()
        .take(NAMED)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if left_out.len() > NAMED {
        said.push_str(&format!(" and {} more", left_out.len() - NAMED));
    }
    said
}

/// The blocks of `spec` near `module`, by tier: 0 the automaton, 1 the actions that run it, 2 the actions its answers
/// make, 3 the fields those and 4 use, 4 the actions out of the states 1 and 2 come from and reach, 5 the rest that is
/// not an action or a field (invariants, timers). `None` when no action of it runs the module.
fn near_module(
    module: &str,
    spec: &SpecSource,
    blocks: &[Block],
) -> Option<Vec<(u8, usize, String)>> {
    let parsed: toml::Value = toml::from_str(&spec.source).ok()?;
    let actions = parsed.get("action").and_then(toml::Value::as_array)?;
    let action_blocks: Vec<usize> = indexes(blocks, "action");
    let runs: BTreeSet<&str> = actions
        .iter()
        .filter(|action| triggers_of(action, module).next().is_some())
        .filter_map(|action| action.get("name")?.as_str())
        .collect();
    if runs.is_empty() {
        return None;
    }
    if action_blocks.len() != actions.len() {
        // Written in a way this reading cannot pair with its actions: the whole spec, nearest first.
        return Some(whole(spec, blocks));
    }
    let answers: BTreeSet<&str> = actions
        .iter()
        .flat_map(|action| triggers_of(action, module))
        .flat_map(|trigger| ["on_success", "on_failure"].map(|key| trigger.get(key)))
        .filter_map(|value| value?.as_str())
        .collect();
    let mut states: BTreeSet<&str> = BTreeSet::new();
    for action in actions
        .iter()
        .filter(|a| named_in(a, &runs) || named_in(a, &answers))
    {
        states.extend(froms(action));
        states.extend(action.get("to").and_then(toml::Value::as_str));
    }
    let around = |action: &toml::Value| froms(action).any(|from| states.contains(from));

    let ty = &spec.entity_type;
    let mut near = Vec::new();
    let mut used_text = String::new();
    for (block, action) in action_blocks.iter().zip(actions) {
        let name = action
            .get("name")
            .and_then(toml::Value::as_str)
            .unwrap_or_default();
        let tier = if runs.contains(name) {
            1
        } else if answers.contains(name) {
            2
        } else if around(action) {
            4
        } else {
            continue;
        };
        used_text.push_str(&text_of(&blocks[*block]));
        near.push((tier, *block, format!("{ty}'s action {name}")));
    }
    let fields = parsed.get("state").and_then(toml::Value::as_array);
    let field_blocks = indexes(blocks, "state");
    if let Some(fields) = fields.filter(|fields| fields.len() == field_blocks.len()) {
        for (block, field) in field_blocks.iter().zip(fields) {
            let name = field
                .get("name")
                .and_then(toml::Value::as_str)
                .unwrap_or_default();
            if !name.is_empty() && mentions(&used_text, name) {
                near.push((3, *block, format!("{ty}'s field {name}")));
            }
        }
    }
    for (block, each) in blocks.iter().enumerate() {
        match each.kind {
            "" | "automaton" => near.push((0, block, format!("{ty}'s states"))),
            "action" | "state" => {}
            other => near.push((5, block, format!("{ty}'s {other}"))),
        }
    }
    Some(near)
}

/// Every block of a spec no action of which runs the module by name: its automaton first, then its actions, its
/// fields, and the rest.
fn whole(spec: &SpecSource, blocks: &[Block]) -> Vec<(u8, usize, String)> {
    let ty = &spec.entity_type;
    blocks
        .iter()
        .enumerate()
        .map(|(block, each)| match each.kind {
            "" | "automaton" => (0, block, format!("{ty}'s states")),
            "action" => (
                1,
                block,
                format!(
                    "{ty}'s {}",
                    name_in(each).map_or("action".to_string(), |n| format!("action {n}"))
                ),
            ),
            "state" => (
                3,
                block,
                format!(
                    "{ty}'s {}",
                    name_in(each).map_or("field".to_string(), |n| format!("field {n}"))
                ),
            ),
            other => (5, block, format!("{ty}'s {other}")),
        })
        .collect()
}

/// A spec's top-level tables, as written: each starts at a header with no dot (`[[action]]`), and a header with one
/// (`[[action.triggers]]`) stays in the table above it. What comes before the first header is a block of kind "".
fn blocks_of(source: &str) -> Vec<Block<'_>> {
    let mut blocks = vec![Block {
        kind: "",
        lines: Vec::new(),
    }];
    for line in source.lines() {
        match header(line) {
            Some(name) if !name.contains('.') => blocks.push(Block {
                kind: name,
                lines: vec![line],
            }),
            _ => {
                if let Some(last) = blocks.last_mut() {
                    last.lines.push(line);
                }
            }
        }
    }
    blocks
}

/// The table a line opens (`action` for `[[action]]`, `action.triggers` for `[[action.triggers]]`), if it is a header.
fn header(line: &str) -> Option<&str> {
    let text = line.trim();
    let inner = text
        .strip_prefix("[[")
        .and_then(|rest| rest.strip_suffix("]]"))
        .or_else(|| {
            text.strip_prefix('[')
                .and_then(|rest| rest.strip_suffix(']'))
        })?
        .trim();
    let named = !inner.is_empty()
        && inner
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.');
    named.then_some(inner)
}

/// A block as Jev reads it: no comment lines or blank lines, each line cut as a module's is.
fn text_of(block: &Block) -> String {
    block
        .lines
        .iter()
        .filter(|line| {
            let text = line.trim();
            !text.is_empty() && !text.starts_with('#')
        })
        .map(|line| cut(line.trim_end()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn indexes(blocks: &[Block], kind: &str) -> Vec<usize> {
    blocks
        .iter()
        .enumerate()
        .filter(|(_, block)| block.kind == kind)
        .map(|(index, _)| index)
        .collect()
}

/// The `name = "..."` a block's table gives itself.
fn name_in(block: &Block) -> Option<String> {
    let parsed: toml::Value = toml::from_str(&block.lines[1..].join("\n")).ok()?;
    Some(parsed.get("name")?.as_str()?.to_string())
}

fn triggers_of<'a>(
    action: &'a toml::Value,
    module: &'a str,
) -> impl Iterator<Item = &'a toml::Value> + 'a {
    action
        .get("triggers")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter(move |trigger| trigger.get("module").and_then(toml::Value::as_str) == Some(module))
}

fn froms(action: &toml::Value) -> impl Iterator<Item = &str> {
    action
        .get("from")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(toml::Value::as_str)
}

fn named_in(action: &toml::Value, names: &BTreeSet<&str>) -> bool {
    action
        .get("name")
        .and_then(toml::Value::as_str)
        .is_some_and(|name| names.contains(name))
}

/// Whether `text` names `word` as a word of its own (`asks`, not `tasks`).
fn mentions(text: &str, word: &str) -> bool {
    text.match_indices(word).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + word.len()..].chars().next();
        let part = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
        !part(before) && !part(after)
    })
}
