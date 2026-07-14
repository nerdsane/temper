pub(super) fn contains_sequence(sql: &str, sequences: &[&[&str]]) -> Option<usize> {
    let tokens = tokens(sql);
    sequences.iter().position(|sequence| {
        tokens.windows(sequence.len()).any(|window| {
            window
                .iter()
                .zip(*sequence)
                .all(|(left, right)| left == right)
        })
    })
}

fn tokens(sql: &str) -> Vec<String> {
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
                tokens.push(sql[start..cursor].to_ascii_uppercase());
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
    use super::contains_sequence;

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
}
