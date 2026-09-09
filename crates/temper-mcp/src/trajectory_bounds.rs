//! Capture bounds and stdio framing for MCP trajectory recording (ARN-222).
//!
//! These are the pure, side-effect-free guards that keep an attacker-controlled
//! MCP session from growing the recorded trajectory or the stdio read buffer
//! without bound. They are split out of `runtime.rs` so the bounds (and their
//! tests) live in one auditable place.

use serde_json::Value;
use std::collections::BTreeMap;
use tokio::io::{self, AsyncBufRead, AsyncBufReadExt, AsyncReadExt};

/// Per-message cap on recorded trajectory text (submitted code / execution result).
pub(crate) const MAX_TRAJECTORY_TEXT_BYTES: usize = 16 * 1024;
/// Cap on the number of turns recorded in a single trajectory.
pub(crate) const MAX_TRAJECTORY_TURNS: usize = 500;
/// Cap on the number of extracted actions embedded in a single turn's decision.
pub(crate) const MAX_TRAJECTORY_ACTIONS: usize = 64;
/// Cap on the number of distinct tenants / entity types tracked per session.
pub(crate) const MAX_SEEN_KEYS: usize = 256;
/// Cap on the byte length of a single tracked observability key (tenant / entity
/// type). Distinct-key *count* alone does not bound memory: 256 near-1-MiB keys
/// would still retain hundreds of MiB, so an oversized key is dropped rather than
/// stored (ARN-222).
pub(crate) const MAX_SEEN_KEY_BYTES: usize = 256;
/// Cap on the total *serialized* JSON size of the trajectory, kept under the
/// server's 2 MiB ingest limit (`POST /api/ots/trajectories` uses Axum's default
/// `DEFAULT_LIMIT` of 2_097_152) so a session that is within the per-turn and
/// per-count caps cannot still produce a trajectory the server rejects with 413 —
/// which the client treats as non-retryable and would silently drop, suppressing
/// the audit trail (ARN-222).
///
/// The budget is metered against a conservative estimate of each turn's serialized
/// contribution (JSON-escaped text, embedded actions, a duplicated error field,
/// and a fixed per-turn envelope), not raw string bytes — raw bytes undercount the
/// wire size by 2x or more once JSON escaping and the OTS envelope are included.
pub(crate) const MAX_TRAJECTORY_TOTAL_BYTES: usize = 1_800_000;

/// Conservative fixed overhead charged to each recorded turn's budget: the OTS
/// turn/message/decision envelope (ids, timestamps, roles, choice label, decision
/// wrapper). Deliberately larger than the real ~600–800 B so metering never
/// undercounts the serialized turn.
pub(crate) const TRAJECTORY_TURN_ENVELOPE_BYTES: usize = 2048;

/// The number of bytes `text` occupies once serialized as a JSON string value,
/// including surrounding quotes and escaping. Used to meter a turn's real wire
/// cost against the ingest budget rather than its raw byte length.
pub(crate) fn json_string_cost(text: &str) -> usize {
    serde_json::to_string(text)
        .map(|s| s.len())
        .unwrap_or(text.len() + 2)
}

/// The number of bytes `value` occupies once serialized as JSON.
pub(crate) fn json_value_cost(value: &Value) -> usize {
    serde_json::to_string(value).map(|s| s.len()).unwrap_or(0)
}
/// Maximum bytes accepted for a single stdio JSON-RPC frame. A peer that never
/// sends a newline would otherwise make a line reader buffer the whole stream
/// into one allocation.
pub(crate) const MAX_STDIO_LINE_BYTES: usize = 1_048_576;

/// Bound the `trajectory_actions` embedded in a turn's decision. The actions are
/// parsed from the full (untruncated) code, so both their count and total
/// serialized size are capped, replacing the array with a summary when it would
/// exceed the text budget — otherwise a large `temper.action(params=...)` dict
/// bypasses the per-message cap every turn (ARN-222).
pub(crate) fn bounded_trajectory_actions(mut actions: Vec<Value>) -> Value {
    actions.truncate(MAX_TRAJECTORY_ACTIONS);
    let value = Value::Array(actions);
    let too_large = serde_json::to_string(&value)
        .map(|s| s.len() > MAX_TRAJECTORY_TEXT_BYTES)
        .unwrap_or(true);
    if too_large {
        return serde_json::json!({
            "trajectory_actions_truncated": true,
            "note": "actions omitted: serialized size exceeded the trajectory cap",
        });
    }
    value
}

/// The tenant a captured trajectory is stored under. It must be the session's
/// authenticated `identity` — never a tenant derived from the executed code,
/// which is attacker-controlled and would let a session file its trajectory under
/// another tenant (ARN-222).
pub(crate) fn trajectory_storage_tenant(identity: &str) -> &str {
    identity
}

/// Increment `key`'s count in a per-session observability map, bounding both the
/// number of distinct keys (`MAX_SEEN_KEYS`) and each key's byte length
/// (`MAX_SEEN_KEY_BYTES`). Returns `true` only when a new key was inserted, so a
/// caller can emit a once-per-key signal without re-firing after the map
/// saturates.
pub(crate) fn bump_seen(map: &mut BTreeMap<String, usize>, key: String) -> bool {
    if let Some(count) = map.get_mut(&key) {
        *count += 1;
        false
    } else if key.len() <= MAX_SEEN_KEY_BYTES && map.len() < MAX_SEEN_KEYS {
        map.insert(key, 1);
        true
    } else {
        false
    }
}

/// Largest byte index `<= max` that is a UTF-8 char boundary of `s`, so
/// `&s[..floor_char_boundary(s, max)]` never panics on a multibyte char.
pub(crate) fn floor_char_boundary(s: &str, max: usize) -> usize {
    let mut end = max.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

/// Truncate recorded trajectory text to `MAX_TRAJECTORY_TEXT_BYTES`, appending an
/// explicit marker, so a large submitted code blob or result can't grow the
/// trajectory without bound (ARN-222).
pub(crate) fn truncate_trajectory_text(text: &str) -> String {
    if text.len() <= MAX_TRAJECTORY_TEXT_BYTES {
        return text.to_string();
    }
    let end = floor_char_boundary(text, MAX_TRAJECTORY_TEXT_BYTES);
    format!("{}…[truncated {} bytes]", &text[..end], text.len() - end)
}

/// Outcome of reading one newline-delimited frame from stdin.
pub(crate) enum StdioFrame {
    /// A complete frame within budget (may include the trailing newline).
    Line(Vec<u8>),
    /// The frame exceeded `MAX_STDIO_LINE_BYTES`; it was drained and dropped.
    TooLarge,
    /// End of input.
    Eof,
}

/// Read one JSON-RPC frame, holding at most `MAX_STDIO_LINE_BYTES + 1` bytes at a
/// time.
///
/// `#365` flagged unbounded stdio reads but checked the line length only *after*
/// `next_line()` had already buffered the whole line, so it did not bound memory.
/// This reads through a `take`-limited reader so no single read allocates past the
/// budget, and drains an oversized frame to the next newline (reusing the same
/// buffer, not a second one) so peak memory stays near one budget rather than two.
pub(crate) async fn read_stdio_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> io::Result<StdioFrame> {
    let mut buf = Vec::new();
    let read = reader
        .take((MAX_STDIO_LINE_BYTES as u64) + 1)
        .read_until(b'\n', &mut buf)
        .await?;
    if read == 0 {
        return Ok(StdioFrame::Eof);
    }
    // A complete frame ends with '\n'. Missing a trailing newline is oversized
    // only when we also consumed the full budget (`buf.len() > cap`); a shorter
    // no-newline read is the final EOF-terminated frame and is kept. This keeps a
    // frame of exactly `MAX_STDIO_LINE_BYTES` content bytes plus its newline
    // (buf.len == cap + 1, ends in '\n') as a valid line rather than rejecting it.
    if buf.last() != Some(&b'\n') && buf.len() > MAX_STDIO_LINE_BYTES {
        // Drain the rest of this oversized frame (bounded per chunk, reusing the
        // buffer) before rejecting it, so the loop resynchronizes on the next
        // frame without holding a second budget's worth of memory.
        loop {
            buf.clear();
            let n = reader
                .take((MAX_STDIO_LINE_BYTES as u64) + 1)
                .read_until(b'\n', &mut buf)
                .await?;
            if n == 0 || buf.last() == Some(&b'\n') {
                break;
            }
        }
        return Ok(StdioFrame::TooLarge);
    }
    Ok(StdioFrame::Line(buf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    fn frame_label(frame: &StdioFrame) -> &'static str {
        match frame {
            StdioFrame::Line(_) => "Line",
            StdioFrame::TooLarge => "TooLarge",
            StdioFrame::Eof => "Eof",
        }
    }

    #[tokio::test]
    async fn stdio_frame_reads_normal_line() {
        let data = b"{\"jsonrpc\":\"2.0\"}\n".to_vec();
        let mut reader = BufReader::new(&data[..]);
        match read_stdio_frame(&mut reader).await.unwrap() {
            StdioFrame::Line(buf) => assert_eq!(buf, data),
            other => panic!("expected a line, got {}", frame_label(&other)),
        }
        assert!(matches!(
            read_stdio_frame(&mut reader).await.unwrap(),
            StdioFrame::Eof
        ));
    }

    #[tokio::test]
    async fn stdio_frame_rejects_oversized_and_resyncs() {
        let mut data = vec![b'x'; MAX_STDIO_LINE_BYTES + 10];
        data.push(b'\n');
        data.extend_from_slice(b"{\"ok\":true}\n");
        let mut reader = BufReader::new(&data[..]);

        assert!(
            matches!(
                read_stdio_frame(&mut reader).await.unwrap(),
                StdioFrame::TooLarge
            ),
            "oversized frame must be rejected"
        );
        match read_stdio_frame(&mut reader).await.unwrap() {
            StdioFrame::Line(buf) => assert_eq!(buf, b"{\"ok\":true}\n"),
            other => panic!("expected the following frame, got {}", frame_label(&other)),
        }
    }

    #[tokio::test]
    async fn stdio_frame_accepts_exact_budget_with_newline() {
        let mut data = vec![b'a'; MAX_STDIO_LINE_BYTES];
        data.push(b'\n');
        let mut reader = BufReader::new(&data[..]);
        match read_stdio_frame(&mut reader).await.unwrap() {
            StdioFrame::Line(buf) => assert_eq!(buf.len(), MAX_STDIO_LINE_BYTES + 1),
            other => panic!(
                "exact-budget frame must be a line, got {}",
                frame_label(&other)
            ),
        }
    }

    #[tokio::test]
    async fn stdio_frame_keeps_final_line_without_newline() {
        let data = b"{\"final\":true}".to_vec();
        let mut reader = BufReader::new(&data[..]);
        match read_stdio_frame(&mut reader).await.unwrap() {
            StdioFrame::Line(buf) => assert_eq!(buf, data),
            other => panic!(
                "EOF-terminated frame must be a line, got {}",
                frame_label(&other)
            ),
        }
    }

    #[test]
    fn bump_seen_bounds_count_and_key_length() {
        let mut map = BTreeMap::new();
        assert!(bump_seen(&mut map, "a".to_string()), "first insert is new");
        assert!(!bump_seen(&mut map, "a".to_string()), "repeat is not new");
        assert_eq!(map.get("a"), Some(&2));

        // Oversized key is dropped, not stored.
        let huge = "x".repeat(MAX_SEEN_KEY_BYTES + 1);
        assert!(!bump_seen(&mut map, huge.clone()));
        assert!(!map.contains_key(&huge));

        // Distinct-key count is capped.
        for i in 0..MAX_SEEN_KEYS + 10 {
            bump_seen(&mut map, format!("k{i}"));
        }
        assert!(map.len() <= MAX_SEEN_KEYS);
    }

    #[test]
    fn truncate_trajectory_text_is_multibyte_safe() {
        let s = "日本語".repeat(20_000); // well over the byte budget
        let out = truncate_trajectory_text(&s);
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
        assert!(out.contains("[truncated"));
    }
}
