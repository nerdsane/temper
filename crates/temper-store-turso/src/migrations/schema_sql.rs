use std::collections::BTreeSet;

pub(super) const RESTRICTED_TABLE_SEQUENCES: &[(&str, &[&str])] = &[
    ("AUTOINCREMENT", &["AUTOINCREMENT"]),
    ("CHECK", &["CHECK"]),
    ("COLLATE", &["COLLATE"]),
    ("DEFERRABLE", &["DEFERRABLE"]),
    ("GENERATED", &["GENERATED"]),
    ("INITIALLY", &["INITIALLY"]),
    ("ON CONFLICT", &["ON", "CONFLICT"]),
    ("STRICT", &["STRICT"]),
    ("WITHOUT ROWID", &["WITHOUT", "ROWID"]),
];

pub(super) fn contains_sequence(sql: &str, sequences: &[&[&str]]) -> Option<usize> {
    let tokens = tokens(sql);
    sequences
        .iter()
        .position(|sequence| contains_tokens(&tokens, sequence))
}

pub(super) fn restricted_table_semantics(sql: &str) -> BTreeSet<String> {
    let tokens = tokens(sql);
    RESTRICTED_TABLE_SEQUENCES
        .iter()
        .filter(|(_, sequence)| contains_tokens(&tokens, sequence))
        .map(|(name, _)| (*name).to_string())
        .collect()
}

pub(super) fn predicate_after_where(sql: &str) -> Option<String> {
    token_spans(sql)
        .into_iter()
        .find(|token| token.value == "WHERE")
        .map(|token| normalize_sql_fragment(&sql[token.end..]))
}

pub(super) fn normalize_schema_ddl(sql: &str) -> String {
    let mut tokens = canonical_tokens(sql);
    if tokens.last().is_some_and(|token| token == ";") {
        tokens.pop();
    }
    if tokens.len() >= 5
        && tokens[0] == "create"
        && tokens[1] == "table"
        && tokens[2] == "if"
        && tokens[3] == "not"
        && tokens[4] == "exists"
    {
        tokens.drain(2..5);
    }
    tokens.join(" ")
}

fn normalize_sql_fragment(sql: &str) -> String {
    let mut tokens = canonical_tokens(sql);
    if tokens.last().is_some_and(|token| token == ";") {
        tokens.pop();
    }
    tokens.join(" ")
}

pub(super) fn canonical_tokens(sql: &str) -> Vec<String> {
    let bytes = sql.as_bytes();
    let mut tokens = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\'' | b'"' | b'`' => {
                let end = skip_quoted(bytes, cursor, bytes[cursor]);
                tokens.push(sql[cursor..end].to_string());
                cursor = end;
            }
            b'[' => {
                let end = skip_quoted(bytes, cursor, b']');
                tokens.push(sql[cursor..end].to_string());
                cursor = end;
            }
            b'-' if bytes.get(cursor + 1) == Some(&b'-') => {
                cursor += 2;
                while cursor < bytes.len() && bytes[cursor] != b'\n' {
                    cursor += 1;
                }
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor += 2;
                while cursor + 1 < bytes.len()
                    && !(bytes[cursor] == b'*' && bytes[cursor + 1] == b'/')
                {
                    cursor += 1;
                }
                cursor = (cursor + 2).min(bytes.len());
            }
            byte if byte.is_ascii_whitespace() => cursor += 1,
            byte if is_identifier_byte(byte) => {
                let start = cursor;
                cursor += 1;
                while cursor < bytes.len() && is_identifier_byte(bytes[cursor]) {
                    cursor += 1;
                }
                tokens.push(sql[start..cursor].to_ascii_lowercase());
            }
            byte => {
                tokens.push((byte as char).to_string());
                cursor += 1;
            }
        }
    }
    tokens
}

fn contains_tokens(tokens: &[String], sequence: &[&str]) -> bool {
    tokens.windows(sequence.len()).any(|window| {
        window
            .iter()
            .zip(sequence)
            .all(|(left, right)| left == right)
    })
}

fn tokens(sql: &str) -> Vec<String> {
    token_spans(sql)
        .into_iter()
        .map(|token| token.value)
        .collect()
}

struct TokenSpan {
    value: String,
    end: usize,
}

fn token_spans(sql: &str) -> Vec<TokenSpan> {
    let bytes = sql.as_bytes();
    let mut tokens = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\'' | b'"' | b'`' => cursor = skip_quoted(bytes, cursor, bytes[cursor]),
            b'[' => cursor = skip_quoted(bytes, cursor, b']'),
            b'-' if bytes.get(cursor + 1) == Some(&b'-') => {
                cursor += 2;
                while cursor < bytes.len() && bytes[cursor] != b'\n' {
                    cursor += 1;
                }
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                cursor += 2;
                while cursor + 1 < bytes.len()
                    && !(bytes[cursor] == b'*' && bytes[cursor + 1] == b'/')
                {
                    cursor += 1;
                }
                cursor = (cursor + 2).min(bytes.len());
            }
            byte if is_identifier_byte(byte) => {
                let start = cursor;
                cursor += 1;
                while cursor < bytes.len() && is_identifier_byte(bytes[cursor]) {
                    cursor += 1;
                }
                tokens.push(TokenSpan {
                    value: sql[start..cursor].to_ascii_uppercase(),
                    end: cursor,
                });
            }
            _ => cursor += 1,
        }
    }
    tokens
}

fn skip_quoted(bytes: &[u8], mut cursor: usize, terminator: u8) -> usize {
    cursor += 1;
    while cursor < bytes.len() {
        if bytes[cursor] != terminator {
            cursor += 1;
            continue;
        }
        if bytes.get(cursor + 1) == Some(&terminator) {
            cursor += 2;
            continue;
        }
        return cursor + 1;
    }
    cursor
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$') || !byte.is_ascii()
}

#[cfg(test)]
mod tests {
    use super::{
        contains_sequence, normalize_schema_ddl, predicate_after_where, restricted_table_semantics,
    };

    #[test]
    fn token_matching_handles_punctuation_and_ignores_quoted_text() {
        let sequences: &[&[&str]] = &[&["CHECK"], &["WITHOUT", "ROWID"]];
        assert_eq!(
            contains_sequence("data TEXT,CHECK(length(data)>0)", sequences),
            Some(0)
        );
        assert_eq!(
            contains_sequence("value TEXT)WITHOUT/* gap */ROWID", sequences),
            Some(1)
        );
        assert_eq!(
            contains_sequence("value TEXT DEFAULT 'CHECK WITHOUT ROWID'", sequences),
            None
        );
        assert_eq!(contains_sequence("\"CHECK\" TEXT", sequences), None);
    }

    #[test]
    fn restricted_semantics_are_ordered_and_ignore_quoted_text() {
        assert_eq!(
            restricted_table_semantics(
                "CREATE TABLE sample (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    note TEXT DEFAULT 'CHECK STRICT',
                    CHECK(length(note) > 0)
                 ) STRICT"
            )
            .into_iter()
            .collect::<Vec<_>>(),
            vec!["AUTOINCREMENT", "CHECK", "STRICT"]
        );
    }

    #[test]
    fn partial_predicate_handles_compact_syntax_and_quoted_where() {
        assert_eq!(
            predicate_after_where(
                "CREATE INDEX sample ON records(json_extract(value, '$.where'))WHERE(length(id)>0);"
            )
            .as_deref(),
            Some("( length ( id ) > 0 )")
        );
    }

    #[test]
    fn schema_ddl_normalization_ignores_formatting_but_preserves_literals() {
        assert_eq!(
            normalize_schema_ddl(
                "CREATE TABLE IF NOT EXISTS sample (
                    value TEXT NOT NULL DEFAULT ('A B'), CHECK(length(value) > 0)
                 );"
            ),
            normalize_schema_ddl(
                "create table sample(value text not null default('A B'),check(length(value)>0))"
            )
        );
        assert_ne!(
            normalize_schema_ddl("CREATE TABLE sample(value TEXT DEFAULT 'A B')"),
            normalize_schema_ddl("CREATE TABLE sample(value TEXT DEFAULT 'a b')")
        );
        assert_ne!(
            normalize_schema_ddl("CREATE TABLE sample(id INTEGER PRIMARY KEY)"),
            normalize_schema_ddl("CREATE TABLE sample(id INTEGER PRIMARYKEY)")
        );
    }
}
