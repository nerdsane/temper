use super::TlaExtractError;

pub(super) fn extract_module_name(source: &str) -> Result<String, TlaExtractError> {
    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("---- MODULE") {
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            if parts.len() >= 3 {
                return Ok(parts[2].trim_end_matches('-').trim().to_string());
            }
        }
    }

    Err(TlaExtractError::NoModule)
}

pub(super) fn extract_list_after(source: &str, keyword: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut in_section = false;

    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with(keyword) {
            in_section = true;
            collect_list_items(trimmed.strip_prefix(keyword).unwrap_or(""), &mut result);
            continue;
        }

        if !in_section {
            continue;
        }

        if trimmed.is_empty() || trimmed.starts_with("VARIABLE") || trimmed.starts_with("----") {
            break;
        }

        let without_comment = trimmed.split("\\*").next().unwrap_or(trimmed);
        collect_identifier_items(without_comment, &mut result);
    }

    result
}

pub(super) fn extract_quoted_string(input: &str) -> Option<String> {
    let start = input.find('"')?;
    let rest = &input[start + 1..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

pub(super) fn extract_inline_set(input: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch != '"' {
            continue;
        }

        let mut value = String::new();
        for ch in chars.by_ref() {
            if ch == '"' {
                break;
            }
            value.push(ch);
        }

        if !value.is_empty() {
            result.push(value);
        }
    }

    result
}

fn collect_list_items(fragment: &str, out: &mut Vec<String>) {
    for item in fragment.split(',') {
        let item = item.trim().trim_matches(|c| c == '\\' || c == '*').trim();
        if !item.is_empty() {
            out.push(item.to_string());
        }
    }
}

fn collect_identifier_items(fragment: &str, out: &mut Vec<String>) {
    for item in fragment.split(',') {
        let item = item
            .trim()
            .trim_matches(|c: char| !c.is_alphanumeric() && c != '_');
        if !item.is_empty() {
            out.push(item.to_string());
        }
    }
}
