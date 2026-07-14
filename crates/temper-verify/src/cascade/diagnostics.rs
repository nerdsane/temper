//! Unsupported-safety diagnostics and source-span helpers (ADR-0178).

use crate::model::{InvariantKind, TemperModel};

/// Stable error code for unsupported safety-invariant diagnostics (ADR-0178).
pub const UNSUPPORTED_SAFETY_INVARIANT_CODE: &str = "VERIFY_UNSUPPORTED_SAFETY_INVARIANT";

/// Byte and 1-based line/column span into the submitted IOA document.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SourceSpan {
    /// Inclusive start offset in UTF-8 bytes.
    pub start_byte: usize,
    /// Exclusive end offset in UTF-8 bytes.
    pub end_byte: usize,
    /// 1-based start line.
    pub start_line: u32,
    /// 1-based start column (UTF-8 bytes within the line).
    pub start_column: u32,
    /// 1-based end line.
    pub end_line: u32,
    /// 1-based end column (UTF-8 bytes within the line; exclusive).
    pub end_column: u32,
}

/// Structured diagnostic for a safety invariant the verifier cannot encode.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UnsupportedInvariantDiagnostic {
    /// Stable machine-readable code ([`UNSUPPORTED_SAFETY_INVARIANT_CODE`]).
    pub code: String,
    /// `[[invariant]]` name from the submitted document.
    pub invariant_name: String,
    /// Original assertion expression that could not be verified.
    pub expression: String,
    /// Source range of the `[[invariant]]` table in the submitted IOA, when found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_span: Option<SourceSpan>,
}

/// Collect ADR-0178 diagnostics for every `Unverifiable` model invariant.
pub(crate) fn collect_unsupported_invariant_diagnostics(
    model: &TemperModel,
    ioa_source: &str,
) -> Vec<UnsupportedInvariantDiagnostic> {
    model
        .invariants
        .iter()
        .filter_map(|inv| {
            if let InvariantKind::Unverifiable { expression } = &inv.kind {
                Some(UnsupportedInvariantDiagnostic {
                    code: UNSUPPORTED_SAFETY_INVARIANT_CODE.to_string(),
                    invariant_name: inv.name.clone(),
                    expression: expression.clone(),
                    source_span: find_invariant_source_span(ioa_source, &inv.name),
                })
            } else {
                None
            }
        })
        .collect()
}

/// Locate the `[[invariant]]` array-table for `name` in the submitted IOA TOML.
fn find_invariant_source_span(source: &str, name: &str) -> Option<SourceSpan> {
    let bytes = source.as_bytes();
    let mut search_from = 0usize;
    while let Some(rel) = source[search_from..].find("[[invariant]]") {
        let table_start = search_from + rel;
        let after_header = table_start + "[[invariant]]".len();
        let next_table = source[after_header..]
            .find("\n[")
            .map(|i| after_header + i)
            .unwrap_or(source.len());
        let table_body = &source[table_start..next_table];
        if invariant_table_name_matches(table_body, name) {
            let end = trim_trailing_ws_end(bytes, next_table);
            return Some(byte_range_to_source_span(source, table_start, end));
        }
        search_from = after_header;
    }
    None
}

fn invariant_table_name_matches(table_body: &str, name: &str) -> bool {
    for line in table_body.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("name") {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix('=') {
                let rest = rest.trim();
                let value = rest
                    .strip_prefix('"')
                    .and_then(|s| s.strip_suffix('"'))
                    .or_else(|| rest.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')));
                if value == Some(name) {
                    return true;
                }
            }
        }
    }
    false
}

fn trim_trailing_ws_end(bytes: &[u8], end: usize) -> usize {
    let mut e = end;
    while e > 0 && matches!(bytes[e - 1], b' ' | b'\t' | b'\n' | b'\r') {
        e -= 1;
    }
    e
}

fn byte_range_to_source_span(source: &str, start_byte: usize, end_byte: usize) -> SourceSpan {
    let (start_line, start_column) = byte_offset_to_line_col(source, start_byte);
    let (end_line, end_column) = byte_offset_to_line_col(source, end_byte);
    SourceSpan {
        start_byte,
        end_byte,
        start_line,
        start_column,
        end_line,
        end_column,
    }
}

fn byte_offset_to_line_col(source: &str, offset: usize) -> (u32, u32) {
    let offset = offset.min(source.len());
    let mut line = 1u32;
    let mut col = 1u32;
    for (i, b) in source.as_bytes().iter().enumerate() {
        if i >= offset {
            break;
        }
        if *b == b'\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}
