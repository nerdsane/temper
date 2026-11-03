//! In debug builds, every statement's plan is checked once: a plan must not
//! search a table by fewer `= ?` columns than one of the table's indexes would
//! take from the statement.
//!
//! Without table statistics, turso_core (0.7.2 to at least 0.9.0-pre.1)
//! counts an equality on any column of a composite primary key as unique
//! (`translate/optimizer/constraints.rs`, `estimate_selectivity`). Every index
//! that starts with the same columns then ties at one row, and the planner may
//! take a narrower one that matches only `tenant` and `entity_type`: a lookup
//! by one record reads every row of its type. On 27 Oct 2026 that lookup took
//! 11 ms at 33,000 field-index rows, against 0.02 ms by the key, inside every
//! append. A statement like that names the index it is for (`INDEXED BY`);
//! this check fails every test that runs one that does not.
//!
//! A join gives a column the same way: `c.entity_id = h.entity_id`, once the
//! plan has read `h`, fixes `c.entity_id` for each of `h`'s rows. On 30 Oct
//! 2026 the boot comparison joined each journal head to its catalog row by
//! tenant and type only, reading every catalog row of the type per record:
//! 20 s at 900 records with 100 KB rows, against 38 ms by the key.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, OnceLock};

use crate::sql_terms::{find_word, gives, joined_with};

/// Statement texts already checked in this process.
fn checked() -> &'static Mutex<BTreeSet<String>> {
    static CHECKED: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    CHECKED.get_or_init(Mutex::default)
}

/// Panic when `sql`'s plan on `conn` searches a table by fewer `= ?` columns
/// than one of the table's indexes would take from `sql`, by a parameter or
/// by a table the plan reads before it.
pub(super) async fn check(conn: &turso::Connection, sql: &str) {
    let head = sql
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    if !matches!(
        head.as_str(),
        "SELECT" | "WITH" | "UPDATE" | "DELETE" | "INSERT"
    ) {
        return;
    }
    if !checked()
        .lock()
        .expect("plan check set")
        .insert(sql.to_string())
    {
        return;
    }
    // A statement the engine cannot plan fails when it runs.
    let Ok(plan) = rows(conn, &format!("EXPLAIN QUERY PLAN {sql}"), 3).await else {
        return;
    };
    let tables = table_names(sql);
    let terms = without_set_lists(sql);
    for (at, detail) in plan.iter().enumerate() {
        let Some((name, searched_by)) = search_of(detail) else {
            continue;
        };
        // A name that more than one plan line reads (each subquery with its
        // own `d`) is left out of joins: which reading a term means is not
        // known.
        let read_once = |other: &str| plan.iter().filter(|line| reads(line, other)).count() == 1;
        let read_before =
            |other: &str| read_once(other) && plan[..at].iter().any(|line| reads(line, other));
        let table = tables.get(name).map_or(name, String::as_str);
        let unqualified = tables.len() == 1;
        let used = searched_by
            .split(" AND ")
            .filter(|term| term.ends_with("=?"))
            .count();
        for (index, columns) in indexes(conn, table).await {
            let given = columns
                .iter()
                .take_while(|column| {
                    gives(&terms, name, unqualified, column)
                        || (read_once(name)
                            && joined_with(&terms, name, column)
                                .into_iter()
                                .any(read_before))
                })
                .count();
            assert!(
                used >= given,
                "this statement gives the first {given} columns of {index} {columns:?} with \
                 `= ?` or a join, but its plan searches {table} by {used} ({detail}); name \
                 the index with INDEXED BY (see driver/plan_check.rs):\n{sql}"
            );
        }
    }
}

/// The text column `column` of every row `sql` answers.
async fn rows(
    conn: &turso::Connection,
    sql: &str,
    column: usize,
) -> Result<Vec<String>, turso::Error> {
    let mut rows = conn.query(sql, ()).await?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().await? {
        if let Ok(turso::Value::Text(text)) = row.get_value(column) {
            out.push(text);
        }
    }
    Ok(out)
}

/// `table`'s indexes that every statement may use, by name, each with its
/// columns in order. A partial index serves only statements that match its
/// condition, so it is left out.
async fn indexes(conn: &turso::Connection, table: &str) -> Vec<(String, Vec<String>)> {
    let Ok(mut list) = conn.query(&format!("PRAGMA index_list({table})"), ()).await else {
        return Vec::new();
    };
    let mut names = Vec::new();
    while let Ok(Some(row)) = list.next().await {
        if let (Ok(turso::Value::Text(name)), Ok(turso::Value::Integer(partial))) =
            (row.get_value(1), row.get_value(4))
            && partial == 0
        {
            names.push(name);
        }
    }
    let mut out = Vec::new();
    for name in names {
        let columns = rows(conn, &format!("PRAGMA index_info({name})"), 2)
            .await
            .unwrap_or_default();
        out.push((name, columns));
    }
    out
}

/// From a plan line such as `SEARCH c USING INDEX i (tenant=? AND
/// entity_type=?)` or `SCAN t`: the table or alias it reads, and what it
/// searches by.
fn search_of(detail: &str) -> Option<(&str, &str)> {
    let rest = detail
        .strip_prefix("SEARCH ")
        .or_else(|| detail.strip_prefix("SCAN "))?;
    let name = rest.split_whitespace().next()?;
    let searched_by = rest
        .split_once('(')
        .and_then(|(_, cols)| cols.split_once(')'))
        .map_or("", |(cols, _)| cols);
    Some((name, searched_by))
}

/// Whether the plan line `detail` reads the table or alias `name`: `SEARCH
/// name ...`, `SCAN name`, or `SCAN table AS name`.
fn reads(detail: &str, name: &str) -> bool {
    let Some(rest) = detail
        .strip_prefix("SEARCH ")
        .or_else(|| detail.strip_prefix("SCAN "))
    else {
        return false;
    };
    let mut words = rest.split_whitespace();
    words.next() == Some(name) || (words.next() == Some("AS") && words.next() == Some(name))
}

/// Every table `sql` reads or writes, by its name and by its alias.
fn table_names(sql: &str) -> BTreeMap<&str, String> {
    const NOT_ALIAS: &[&str] = &[
        "WHERE",
        "SET",
        "ON",
        "LEFT",
        "INNER",
        "CROSS",
        "JOIN",
        "INDEXED",
        "NOT",
        "ORDER",
        "GROUP",
        "LIMIT",
        "VALUES",
        "UNION",
        "USING",
        "DEFAULT",
        "SELECT",
        "RETURNING",
    ];
    let words: Vec<&str> = sql
        .split(|c: char| c.is_whitespace() || matches!(c, ',' | '(' | ')' | ';'))
        .filter(|word| !word.is_empty())
        .collect();
    let mut names = BTreeMap::new();
    for (index, word) in words.iter().enumerate() {
        if !["FROM", "JOIN", "UPDATE", "INTO"].contains(&word.to_ascii_uppercase().as_str()) {
            continue;
        }
        let Some(&table) = words.get(index + 1) else {
            continue;
        };
        if !table.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        names.insert(table, table.to_string());
        let mut next = index + 2;
        if words
            .get(next)
            .is_some_and(|word| word.eq_ignore_ascii_case("AS"))
        {
            next += 1;
        }
        if let Some(&alias) = words.get(next)
            && !NOT_ALIAS.contains(&alias.to_ascii_uppercase().as_str())
            && alias.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            names.insert(alias, table.to_string());
        }
    }
    names
}

/// `sql` with each `SET` list blanked out, up to the next `WHERE`: an
/// assignment there writes a column, it does not choose rows.
fn without_set_lists(sql: &str) -> String {
    let upper = sql.to_ascii_uppercase();
    let mut out = sql.to_string();
    let mut from = 0;
    while let Some(found) = find_word(&upper[from..], "SET") {
        let start = from + found;
        let end = find_word(&upper[start..], "WHERE").map_or(sql.len(), |at| start + at);
        out.replace_range(start..end, &" ".repeat(end - start));
        from = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plan_line_names_what_it_reads_and_searches_by() {
        assert_eq!(
            search_of("SEARCH c USING INDEX idx (tenant=? AND entity_type=?)"),
            Some(("c", "tenant=? AND entity_type=?"))
        );
        assert_eq!(
            search_of("SEARCH k USING INDEX i (tenant=? AND entity_id=?) LEFT-JOIN"),
            Some(("k", "tenant=? AND entity_id=?"))
        );
        assert_eq!(search_of("SCAN events"), Some(("events", "")));
        assert_eq!(search_of("USE TEMP B-TREE FOR ORDER BY"), None);
    }

    #[test]
    fn a_plan_line_reads_its_table_or_alias() {
        assert!(reads("SCAN heads AS h", "h"));
        assert!(reads("SCAN heads AS h", "heads"));
        assert!(reads("SEARCH c USING INDEX i (tenant=?) LEFT-JOIN", "c"));
        assert!(!reads("SEARCH c USING INDEX i (tenant=?)", "i"));
        assert!(!reads("USE SORTER FOR GROUP BY", "h"));
    }

    #[test]
    fn aliases_name_their_tables() {
        let names = table_names(
            "SELECT c.x FROM entity_catalog AS c INDEXED BY i LEFT JOIN counters k ON k.a = c.a \
             WHERE c.tenant = ?1",
        );
        assert_eq!(names.get("c").map(String::as_str), Some("entity_catalog"));
        assert_eq!(names.get("k").map(String::as_str), Some("counters"));
        assert_eq!(
            table_names("DELETE FROM entity_field_index WHERE tenant = ?").len(),
            1
        );
    }

    #[test]
    fn an_assignment_is_not_given() {
        let terms = without_set_lists(
            "UPDATE t SET disposition = ?3, offset = ?4 WHERE tenant = ?1 AND id = ?2",
        );
        assert!(!gives(&terms, "t", true, "disposition"));
        assert!(!gives(&terms, "t", true, "offset"));
        assert!(gives(&terms, "t", true, "tenant"));
        assert!(gives(&terms, "t", true, "id"));
        let upsert = without_set_lists(
            "INSERT INTO t (a) VALUES (?1) ON CONFLICT(a) DO UPDATE SET last_seen = ?2",
        );
        assert!(!gives(&upsert, "t", true, "last_seen"));
        assert_eq!(
            without_set_lists("SELECT reset FROM t"),
            "SELECT reset FROM t"
        );
    }
}
