//! Trajectory capture budgets (ARN-222 / ADR-0166).

/// Max UTF-8 characters retained for trajectory code/results (includes truncation marker).
pub(crate) const MAX_TRAJECTORY_CODE_CHARS: usize = 16_384;
/// Max UTF-8 characters retained for execution results (includes truncation marker).
pub(crate) const MAX_TRAJECTORY_RESULT_CHARS: usize = 16_384;
/// Max stdio JSON-RPC line bytes accepted.
pub(crate) const MAX_STDIO_LINE_BYTES: usize = 1_048_576;

/// Marker appended when truncating; counted against `max_chars`.
const TRUNCATION_MARKER: &str = "…[truncated]";

/// Character-safe truncation with an explicit marker (never panics on multibyte).
///
/// When truncation occurs the returned string's char count is at most `max_chars`
/// (prefix + `TRUNCATION_MARKER`). If `max_chars` is smaller than the marker, the
/// marker alone is returned truncated to `max_chars`.
pub(crate) fn char_safe_truncate(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    let marker_chars = TRUNCATION_MARKER.chars().count();
    if max_chars <= marker_chars {
        return TRUNCATION_MARKER.chars().take(max_chars).collect();
    }
    let keep = max_chars - marker_chars;
    let truncated: String = s.chars().take(keep).collect();
    format!("{truncated}{TRUNCATION_MARKER}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn char_safe_truncate_handles_multibyte() {
        let s = "日本語テスト".repeat(10); // long multibyte input
        let out = char_safe_truncate(&s, 20); // force truncation; marker counts toward budget
        assert!(out.starts_with('日'), "{out}");
        assert!(out.contains("[truncated]"), "{out}");
        assert_eq!(out.chars().count(), 20, "{out}");
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }

    #[test]
    fn char_safe_truncate_respects_budget_including_marker() {
        let s = "abcdefghijklmnopqrstuvwxyz";
        let out = char_safe_truncate(s, 15);
        assert!(
            out.chars().count() <= 15,
            "len={} out={out}",
            out.chars().count()
        );
        assert!(out.contains("[truncated]"), "{out}");
    }
}
