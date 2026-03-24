pub(super) fn parse_kv(line: &str) -> Option<(&str, String)> {
    let eq = line.find('=')?;
    let key = line[..eq].trim();
    let raw_value = line[eq + 1..].trim();
    let value = raw_value.trim_matches('"').trim_matches('\'').to_string();
    Some((key, value))
}

pub(super) fn parse_string_array(value: &str) -> Vec<String> {
    let trimmed = value.trim();
    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        let inner = &trimmed[1..trimmed.len() - 1];
        return split_top_level(inner, ',')
            .into_iter()
            .map(|item| item.trim().trim_matches('"').trim_matches('\'').to_string())
            .filter(|item| !item.is_empty())
            .collect();
    }

    vec![trimmed.trim_matches('"').trim_matches('\'').to_string()]
}

pub(super) fn split_inline_tables(s: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let mut depth: usize = 0;
    let mut start = None;
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut escaped = false;

    for (index, ch) in s.char_indices() {
        if in_double_quote && ch == '\\' {
            escaped = !escaped;
            continue;
        }

        if ch == '"' && !in_single_quote && !escaped {
            in_double_quote = !in_double_quote;
        } else if ch == '\'' && !in_double_quote {
            in_single_quote = !in_single_quote;
        }

        if ch != '\\' {
            escaped = false;
        }

        if in_single_quote || in_double_quote {
            continue;
        }

        match ch {
            '{' => {
                if depth == 0 {
                    start = Some(index);
                }
                depth += 1;
            }
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0
                    && let Some(start_index) = start.take()
                {
                    result.push(&s[start_index..=index]);
                }
            }
            _ => {}
        }
    }

    result
}

pub(super) fn parse_inline_fields(s: &str) -> std::collections::BTreeMap<String, String> {
    let mut map = std::collections::BTreeMap::new();
    for pair in split_top_level(s, ',') {
        let pair = pair.trim();
        if let Some(eq_pos) = pair.find('=') {
            let key = pair[..eq_pos].trim().to_string();
            let val = pair[eq_pos + 1..]
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .to_string();
            map.insert(key, val);
        }
    }
    map
}

/// Join multiline array values into single logical lines.
///
/// When a TOML line has unbalanced brackets (e.g., `effect = [`), this
/// function accumulates subsequent lines until brackets are balanced,
/// producing a single logical line for the parser.
pub(super) fn join_multiline_arrays(input: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut buffer = String::new();
    let mut bracket_depth: i32 = 0;

    for line in input.lines() {
        let trimmed = line.trim();

        if bracket_depth > 0 {
            buffer.push(' ');
            buffer.push_str(trimmed);
            bracket_depth += net_bracket_depth(trimmed);
            if bracket_depth <= 0 {
                result.push(std::mem::take(&mut buffer));
                bracket_depth = 0;
            }
            continue;
        }

        let value_part = trimmed
            .find('=')
            .map(|eq_pos| &trimmed[eq_pos + 1..])
            .unwrap_or(trimmed);
        let depth = net_bracket_depth(value_part);
        if depth > 0 {
            buffer = trimmed.to_string();
            bracket_depth = depth;
        } else {
            result.push(trimmed.to_string());
        }
    }

    if !buffer.is_empty() {
        result.push(buffer);
    }

    result
}

fn split_top_level(s: &str, delimiter: char) -> Vec<&str> {
    let mut result = Vec::new();
    let mut start = 0;
    let mut bracket_depth = 0_i32;
    let mut brace_depth = 0_i32;
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut escaped = false;

    for (index, ch) in s.char_indices() {
        if in_double_quote && ch == '\\' {
            escaped = !escaped;
            continue;
        }

        if ch == '"' && !in_single_quote && !escaped {
            in_double_quote = !in_double_quote;
        } else if ch == '\'' && !in_double_quote {
            in_single_quote = !in_single_quote;
        }

        if ch != '\\' {
            escaped = false;
        }

        if in_single_quote || in_double_quote {
            continue;
        }

        match ch {
            '[' => bracket_depth += 1,
            ']' => bracket_depth -= 1,
            '{' => brace_depth += 1,
            '}' => brace_depth -= 1,
            _ if ch == delimiter && bracket_depth == 0 && brace_depth == 0 => {
                result.push(&s[start..index]);
                start = index + ch.len_utf8();
            }
            _ => {}
        }
    }

    result.push(&s[start..]);
    result
}

fn net_bracket_depth(value: &str) -> i32 {
    value.chars().fold(0, |depth, ch| match ch {
        '[' => depth + 1,
        ']' => depth - 1,
        _ => depth,
    })
}
