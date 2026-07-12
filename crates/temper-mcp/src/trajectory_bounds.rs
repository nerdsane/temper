//! Trajectory capture budgets (ARN-222 / ADR-0163).

/// Max UTF-8 characters retained for trajectory code/results.
pub(crate) const MAX_TRAJECTORY_CODE_CHARS: usize = 16_384;
/// Max UTF-8 characters retained for execution results.
pub(crate) const MAX_TRAJECTORY_RESULT_CHARS: usize = 16_384;
/// Max stdio JSON-RPC line bytes accepted.
pub(crate) const MAX_STDIO_LINE_BYTES: usize = 1_048_576;

/// Character-safe truncation with an explicit marker (never panics on multibyte).
pub(crate) fn char_safe_truncate(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max_chars).collect();
    format!("{truncated}…[truncated]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn char_safe_truncate_handles_multibyte() {
        let s = "日本語テスト";
        let out = char_safe_truncate(s, 2);
        assert!(out.starts_with("日本"), "{out}");
        assert!(out.contains("[truncated]"), "{out}");
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }
}
