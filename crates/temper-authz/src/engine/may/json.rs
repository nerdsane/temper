//! Reading the JSON form of Cedar policies and values.

use serde_json::Value as Json;

/// The one key and value of a JSON expression node.
pub(super) fn single(expr: &Json) -> Option<(&str, &Json)> {
    let object = expr.as_object()?;
    if object.len() != 1 {
        return None;
    }
    object.iter().next().map(|(k, v)| (k.as_str(), v))
}

/// An entity's type and id, from `{"__entity": {...}}` or `{"type", "id"}`.
pub(super) fn entity_parts(value: &Json) -> Option<(&str, &str)> {
    let entity = value.get("__entity").unwrap_or(value);
    Some((entity.get("type")?.as_str()?, entity.get("id")?.as_str()?))
}

pub(super) fn same(a: &Json, b: &Json) -> bool {
    match (entity_parts(a), entity_parts(b)) {
        (Some(x), Some(y)) => x == y,
        (None, None) => match (a, b) {
            // Cedar sets ignore order and repeated members, recursively.
            (Json::Array(a), Json::Array(b)) => {
                a.iter().all(|x| b.iter().any(|y| same(x, y)))
                    && b.iter().all(|y| a.iter().any(|x| same(x, y)))
            }
            (Json::Object(a), Json::Object(b)) => {
                a.len() == b.len()
                    && a.iter()
                        .all(|(key, value)| b.get(key).is_some_and(|other| same(value, other)))
            }
            _ => a == b,
        },
        _ => false,
    }
}

/// Whether `text` matches a `like` pattern: literal characters, and
/// wildcards that match any run of characters.
pub(super) fn like(text: &str, pattern: &[Json]) -> bool {
    let text: Vec<char> = text.chars().collect();
    // Which prefixes of `text` the pattern so far can match.
    let mut matched = vec![false; text.len() + 1];
    matched[0] = true;
    for element in pattern {
        match element.get("Literal").and_then(Json::as_str) {
            Some(literal) => {
                let literal: Vec<char> = literal.chars().collect();
                let mut next = vec![false; text.len() + 1];
                for (at, &yes) in matched.iter().enumerate() {
                    let end = at + literal.len();
                    if yes && end <= text.len() && text[at..end] == literal[..] {
                        next[end] = true;
                    }
                }
                matched = next;
            }
            None => {
                let first = matched.iter().position(|&yes| yes);
                matched = (0..=text.len())
                    .map(|at| first.is_some_and(|f| at >= f))
                    .collect();
            }
        }
    }
    matched[text.len()]
}
