//! Which columns a statement fixes: a `column = ?` term that every row it
//! answers must meet. The debug plan check (`driver/plan_check.rs`) holds a
//! plan to the indexes a statement fixes the columns of, and a statement
//! built at run time names its index by the same rule.

/// Whether `sql` gives `column` of `name` with `= ?`: as `name.column`, or
/// bare when the statement reads one table, in a term every row must meet
/// (see [`holds_for_every_row`]).
pub(crate) fn gives(sql: &str, name: &str, unqualified: bool, column: &str) -> bool {
    let bytes = sql.as_bytes();
    let mut from = 0;
    while let Some(found) = sql[from..].find(column) {
        let start = from + found;
        let end = start + column.len();
        from = end;
        let before = &sql[..start];
        let qualified = before.ends_with('.')
            && before[..before.len() - 1].ends_with(name)
            && !before[..before.len() - 1 - name.len()]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
        let bare = unqualified
            && !before
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.');
        let word_ends = !bytes
            .get(end)
            .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_');
        let Some(after) = sql[end..]
            .trim_start()
            .strip_prefix('=')
            .and_then(|after| after.trim_start().strip_prefix('?'))
        else {
            continue;
        };
        let term_start = if qualified {
            start - 1 - name.len()
        } else {
            start
        };
        let term_end = sql.len() - after.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if (qualified || bare) && word_ends && holds_for_every_row(sql, term_start, term_end) {
            return true;
        }
    }
    false
}

/// The tables `sql` equates `name.column` with, each by the name or alias its
/// term uses (`name.column = other.x` or `other.x = name.column`), in terms
/// every row must meet. A plan that reads `other` before `name` has its value
/// for each of its rows, so it can search `name` by `column` as by `= ?`.
pub(crate) fn joined_with<'a>(sql: &'a str, name: &str, column: &str) -> Vec<&'a str> {
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let qualified = format!("{name}.{column}");
    let mut others = Vec::new();
    let mut from = 0;
    while let Some(found) = sql[from..].find(&qualified) {
        let start = from + found;
        let end = start + qualified.len();
        from = end;
        if sql[..start].chars().next_back().is_some_and(is_word)
            || sql[end..].chars().next().is_some_and(is_word)
        {
            continue;
        }
        let right = sql[end..]
            .trim_start()
            .strip_prefix('=')
            .filter(|rest| !rest.starts_with('='))
            .and_then(|rest| {
                let rest = rest.trim_start();
                leading_column(rest).map(|(other, len)| (other, sql.len() - rest.len() + len))
            });
        let left = sql[..start]
            .trim_end()
            .strip_suffix('=')
            .filter(|rest| !rest.ends_with(['<', '>', '!', '=']))
            .and_then(|rest| {
                let rest = rest.trim_end();
                trailing_column(rest).map(|(other, len)| (other, rest.len() - len))
            });
        let terms = [
            right.map(|(other, term_end)| (other, start, term_end)),
            left.map(|(other, term_start)| (other, term_start, end)),
        ];
        for (other, term_start, term_end) in terms.into_iter().flatten() {
            if other != name && holds_for_every_row(sql, term_start, term_end) {
                others.push(other);
            }
        }
    }
    others
}

/// The table of the `table.column` that `text` starts with, and its length.
fn leading_column(text: &str) -> Option<(&str, usize)> {
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let table_len = text.find(|c: char| !is_word(c))?;
    let rest = text[table_len..].strip_prefix('.')?;
    let column_len = rest.find(|c: char| !is_word(c)).unwrap_or(rest.len());
    (table_len > 0 && column_len > 0).then(|| (&text[..table_len], table_len + 1 + column_len))
}

/// The table of the `table.column` that `text` ends with, and its length.
fn trailing_column(text: &str) -> Option<(&str, usize)> {
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let column_start = text.rfind(|c: char| !is_word(c))? + 1;
    let before = text[..column_start].strip_suffix('.')?;
    let table_start = before.rfind(|c: char| !is_word(c)).map_or(0, |at| at + 1);
    (table_start < before.len() && column_start < text.len())
        .then(|| (&before[table_start..], text.len() - table_start))
}

/// Whether the term at `start..end` of `sql` holds for every row: no group of
/// parentheses around it joins terms with `OR` (`(?2 IS NULL OR tenant =
/// ?2)`), and no `NOT` stands right before it or before a group that holds
/// it.
fn holds_for_every_row(sql: &str, start: usize, end: usize) -> bool {
    let bytes = sql.as_bytes();
    let (mut open, mut close) = (start, end);
    loop {
        let left = enclosing(bytes[..open].iter().rev(), b')', b'(').map(|back| open - 1 - back);
        let right = enclosing(bytes[close..].iter(), b'(', b')').map(|ahead| close + ahead);
        let inner = left.map_or(0, |at| at + 1)..right.unwrap_or(bytes.len());
        if find_word(&outermost(&sql[inner.clone()]).to_ascii_uppercase(), "OR").is_some() {
            return false;
        }
        let lead = sql[inner.start..open].trim_end().as_bytes();
        let negated = lead.len() >= 3
            && lead[lead.len() - 3..].eq_ignore_ascii_case(b"NOT")
            && !lead[..lead.len() - 3]
                .last()
                .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_');
        if negated {
            return false;
        }
        match (left, right) {
            (Some(left), Some(right)) => (open, close) = (left, right + 1),
            _ => return true,
        }
    }
}

/// How far into `bytes` the parenthesis `close` that is not matched by an
/// `open` before it stands: walking out of a term, the group around it.
fn enclosing<'a>(bytes: impl Iterator<Item = &'a u8>, open: u8, close: u8) -> Option<usize> {
    let mut depth = 0usize;
    for (at, byte) in bytes.enumerate() {
        if *byte == open {
            depth += 1;
        } else if *byte == close {
            if depth == 0 {
                return Some(at);
            }
            depth -= 1;
        }
    }
    None
}

/// `text` with every group of parentheses in it blanked: what joins its
/// terms at the outermost level.
fn outermost(text: &str) -> String {
    let mut depth = 0usize;
    text.chars()
        .map(|c| match c {
            '(' => {
                depth += 1;
                ' '
            }
            ')' => {
                depth = depth.saturating_sub(1);
                ' '
            }
            c if depth == 0 => c,
            _ => ' ',
        })
        .collect()
}

/// Where `word` first stands as a whole word in `text`.
pub(crate) fn find_word(text: &str, word: &str) -> Option<usize> {
    let is_word = |c: &u8| c.is_ascii_alphanumeric() || *c == b'_';
    let bytes = text.as_bytes();
    let mut from = 0;
    while let Some(found) = text[from..].find(word) {
        let start = from + found;
        let end = start + word.len();
        let word_before = start > 0 && is_word(&bytes[start - 1]);
        if !word_before && !bytes.get(end).is_some_and(is_word) {
            return Some(start);
        }
        from = end;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_column_is_given_only_against_a_parameter() {
        let sql = "SELECT 1 FROM t c JOIN u k ON k.entity_id = c.entity_id \
                   WHERE c.tenant = ?1 AND c.entity_id=?3";
        assert!(gives(sql, "c", false, "tenant"));
        assert!(gives(sql, "c", false, "entity_id"));
        assert!(!gives(sql, "k", false, "entity_id"));
        assert!(!gives(sql, "c", false, "entity_type"));
        let bare = "DELETE FROM t WHERE tenant = ? AND xentity_id = ?";
        assert!(gives(bare, "t", true, "tenant"));
        assert!(!gives(bare, "t", true, "entity_id"));
    }

    #[test]
    fn a_term_joined_by_or_is_not_given() {
        let sql = "SELECT 1 FROM t WHERE session_id = ?1 AND (?2 IS NULL OR tenant = ?2) \
                   AND (entity_type = ?3 OR ?3 IS NULL) AND action = ?4 ORDER BY id";
        assert!(gives(sql, "t", true, "session_id"));
        assert!(!gives(sql, "t", true, "tenant"));
        assert!(!gives(sql, "t", true, "entity_type"));
        assert!(gives(sql, "t", true, "action"));
        let ordered = "SELECT 1 FROM t WHERE tenant = ?1 AND origin = ?2";
        assert!(gives(ordered, "t", true, "origin"));
        let grouped = "SELECT 1 FROM t WHERE (tenant = ?1) OR x = 1";
        assert!(!gives(grouped, "t", true, "tenant"));
        let nested = "x = ?3 OR (y = ?4 AND status = ?5)";
        assert!(!gives(nested, "t", true, "status"));
    }

    #[test]
    fn a_column_equated_with_another_tables_is_joined_with_it() {
        let sql = "SELECT 1 FROM heads h LEFT JOIN entity_catalog c \
                   ON c.tenant = ?1 AND c.entity_type = h.entity_type AND c.entity_id=h.entity_id \
                   LEFT JOIN counters k ON k.tenant = c.tenant AND (k.x = c.x OR k.y = 1) \
                   WHERE c.sequence_nr <= h.sequence_nr AND NOT d.entity_id = c.entity_id";
        assert_eq!(joined_with(sql, "c", "entity_type"), vec!["h"]);
        assert_eq!(joined_with(sql, "c", "entity_id"), vec!["h"]);
        assert_eq!(joined_with(sql, "c", "tenant"), vec!["k"]);
        assert_eq!(joined_with(sql, "k", "tenant"), vec!["c"]);
        assert!(joined_with(sql, "c", "x").is_empty());
        assert!(joined_with(sql, "c", "sequence_nr").is_empty());
        assert!(joined_with(sql, "d", "entity_id").is_empty());
        assert!(joined_with(sql, "h", "entity_type_x").is_empty());
    }

    #[test]
    fn a_negated_term_is_not_given() {
        assert!(!gives("(NOT status = ?3)", "t", true, "status"));
        assert!(!gives(
            "(NOT (x = ?3 AND status = ?4))",
            "t",
            true,
            "status"
        ));
        assert!(gives(
            "(status = ?3 AND (NOT x = ?4)) AND y IS NOT NULL",
            "t",
            true,
            "status"
        ));
    }

    #[test]
    fn a_filter_term_inside_its_groups_is_given() {
        let filter = "((status = ?3 AND entity_id IN (SELECT entity_id FROM \
                      entity_field_index WHERE tenant = ?1 AND field_name = ?4)) AND x = ?5)";
        assert!(gives(filter, "entity_catalog", true, "status"));
        assert!(gives(filter, "entity_catalog", true, "x"));
    }
}
