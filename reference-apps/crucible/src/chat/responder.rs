//! The Crucible Phase 4 turn loop.
//!
//! Given a `session_id` and optionally a new user message, this
//! module drives exactly one turn of a chat: read the session +
//! agent, reconstruct history from the SessionEvent feed, call the
//! model, and write the reply back as new events plus the matching
//! Session bound actions.
//!
//! The loop is **stateless**. Every invocation re-reads the full
//! SessionEvent history from scratch, so a crashed or interrupted
//! turn can be safely re-run — the next invocation will observe
//! whichever events already landed and advance from there. Event
//! ids are derived from the session id + sequence number, so replay
//! either succeeds trivially (idempotent) or 409s on the duplicate
//! id (the correct resume signal).
//!
//! The 12 steps below match the plan in
//! `reference-apps/crucible/PHASE_4_PLAN.md` §"The turn loop".

use crate::chat::anthropic::{ChatMessage, ContentBlock, MessagesRequest, Model};
use crate::chat::temper_client::{
    SessionAction, SessionEventRow, SessionRow, TemperClient,
};
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

/// The default `max_tokens` Phase 4 asks Anthropic for. Intentionally
/// hardcoded — see ADR-0046 Sub-Decision 4 for why we did not add a
/// `MaxTokens` column to `ManagedAgent`.
const DEFAULT_MAX_TOKENS: u32 = 1024;

/// Cap on how many events we will load per turn. The append-only
/// feed is in practice small for chat sessions, but we want an
/// explicit bound so a pathological session cannot hang the CLI.
const EVENT_HISTORY_LIMIT: u32 = 500;

#[derive(Debug, Clone)]
pub struct RespondRequest<'a> {
    pub session_id: &'a str,
    /// If `Some`, the responder appends this text as a new
    /// `user.message` event before calling the model. If `None`,
    /// the loop reacts to whatever user events already sit at the
    /// tail of the feed (the `respond` CLI subcommand path).
    pub new_user_message: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct RespondOutcome {
    pub assistant_text: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    /// The sequence number of the `agent.message` event this turn
    /// emitted. Exposed so the caller (and tests) can correlate.
    pub agent_message_sequence: i64,
}

/// Run one turn of the chat loop against the supplied Temper instance
/// and model provider. Returns the assistant's text plus the token
/// counts observed from the model.
pub async fn respond<M: Model>(
    temper: &TemperClient,
    model: &M,
    req: RespondRequest<'_>,
) -> Result<RespondOutcome> {
    // ------------------------------------------------------------------
    // 1. Fetch parents.
    // ------------------------------------------------------------------
    let session = temper
        .get_session(req.session_id)
        .await
        .with_context(|| format!("loading Session('{}')", req.session_id))?;
    if session.status == "Archived" || session.status == "Terminated" {
        return Err(anyhow!(
            "session {} is in terminal status {}; cannot respond",
            req.session_id,
            session.status
        ));
    }
    let agent = temper
        .get_managed_agent(&session.agent_id)
        .await
        .with_context(|| format!("loading ManagedAgent('{}')", session.agent_id))?;

    // ------------------------------------------------------------------
    // 2. Fetch history and compute the next sequence.
    // ------------------------------------------------------------------
    let mut history = temper
        .list_session_events(req.session_id, EVENT_HISTORY_LIMIT)
        .await
        .with_context(|| format!("listing events for session {}", req.session_id))?;
    history.sort_by_key(|e| e.sequence);
    let mut next_sequence = history.last().map(|e| e.sequence + 1).unwrap_or(0);
    let now = now_rfc3339();

    // ------------------------------------------------------------------
    // 3. Append the new user message, if the caller supplied one.
    // ------------------------------------------------------------------
    if let Some(text) = req.new_user_message {
        let row = SessionEventRow {
            id: event_id(req.session_id, next_sequence),
            session_id: req.session_id.to_string(),
            sequence: next_sequence,
            kind: "user.message".to_string(),
            created_at: now.clone(),
            processed_at: Some(now.clone()),
            content: Some(user_message_content(text)),
            ..blank_event()
        };
        temper
            .create_session_event(&row)
            .await
            .context("POSTing user.message event")?;
        history.push(row);
        next_sequence += 1;
    }

    // ------------------------------------------------------------------
    // 4. Drive lifecycle to Running if needed.
    // ------------------------------------------------------------------
    drive_to_running(temper, &session).await?;

    // ------------------------------------------------------------------
    // 5. Emit span.model_request_start and record its id.
    // ------------------------------------------------------------------
    let model_request_start_id = event_id(req.session_id, next_sequence);
    let start_row = SessionEventRow {
        id: model_request_start_id.clone(),
        session_id: req.session_id.to_string(),
        sequence: next_sequence,
        kind: "span.model_request_start".to_string(),
        created_at: now.clone(),
        processed_at: Some(now.clone()),
        content: Some(
            serde_json::json!({ "model": agent.model_id, "started_at": now }).to_string(),
        ),
        model_speed: agent.model_speed.clone(),
        ..blank_event()
    };
    temper
        .create_session_event(&start_row)
        .await
        .context("POSTing span.model_request_start event")?;
    history.push(start_row);
    next_sequence += 1;

    // ------------------------------------------------------------------
    // 6. Build the Anthropic request from the reconstructed history.
    // ------------------------------------------------------------------
    let messages = events_to_messages(&history)?;
    let req_anthropic = MessagesRequest {
        model: agent.model_id.clone(),
        system: agent.system.clone(),
        messages,
        max_tokens: DEFAULT_MAX_TOKENS,
    };

    // ------------------------------------------------------------------
    // 7. Call the model.
    // ------------------------------------------------------------------
    let response = model
        .complete(req_anthropic)
        .await
        .context("calling the model provider")?;
    let assistant_text = response.text();
    if assistant_text.is_empty() {
        return Err(anyhow!("model returned an empty response"));
    }

    // ------------------------------------------------------------------
    // 8. Emit agent.message.
    // ------------------------------------------------------------------
    let agent_message_sequence = next_sequence;
    let agent_row = SessionEventRow {
        id: event_id(req.session_id, next_sequence),
        session_id: req.session_id.to_string(),
        sequence: next_sequence,
        kind: "agent.message".to_string(),
        created_at: now.clone(),
        processed_at: Some(now.clone()),
        content: Some(agent_message_content(&assistant_text)),
        ..blank_event()
    };
    temper
        .create_session_event(&agent_row)
        .await
        .context("POSTing agent.message event")?;
    next_sequence += 1;

    // ------------------------------------------------------------------
    // 9. Emit span.model_request_end with usage + IsError=false.
    // ------------------------------------------------------------------
    let end_row = SessionEventRow {
        id: event_id(req.session_id, next_sequence),
        session_id: req.session_id.to_string(),
        sequence: next_sequence,
        kind: "span.model_request_end".to_string(),
        created_at: now.clone(),
        processed_at: Some(now.clone()),
        content: Some(
            serde_json::json!({
                "stop_reason": response.stop_reason.clone().unwrap_or_default()
            })
            .to_string(),
        ),
        model_request_start_id: Some(model_request_start_id),
        is_error: Some(false),
        model_input_tokens: Some(response.usage.input_tokens),
        model_output_tokens: Some(response.usage.output_tokens),
        model_cache_creation_input_tokens: Some(response.usage.cache_creation_input_tokens),
        model_cache_read_input_tokens: Some(response.usage.cache_read_input_tokens),
        model_speed: agent.model_speed.clone(),
        ..blank_event()
    };
    temper
        .create_session_event(&end_row)
        .await
        .context("POSTing span.model_request_end event")?;
    next_sequence += 1;

    // ------------------------------------------------------------------
    // 10. Emit session.status_idle with StopReason=end_turn.
    // ------------------------------------------------------------------
    let idle_row = SessionEventRow {
        id: event_id(req.session_id, next_sequence),
        session_id: req.session_id.to_string(),
        sequence: next_sequence,
        kind: "session.status_idle".to_string(),
        created_at: now.clone(),
        processed_at: Some(now.clone()),
        content: Some("{}".to_string()),
        stop_reason: Some("end_turn".to_string()),
        ..blank_event()
    };
    temper
        .create_session_event(&idle_row)
        .await
        .context("POSTing session.status_idle event")?;

    // ------------------------------------------------------------------
    // 11. Transition Running → Idle.
    // ------------------------------------------------------------------
    temper
        .invoke_session_action(req.session_id, SessionAction::IdleSession)
        .await
        .context("invoking IdleSession")?;

    // ------------------------------------------------------------------
    // 12. Return the outcome.
    // ------------------------------------------------------------------
    Ok(RespondOutcome {
        assistant_text,
        input_tokens: response.usage.input_tokens,
        output_tokens: response.usage.output_tokens,
        agent_message_sequence,
    })
}

/// Push the Session into `Running` if it isn't already. From
/// `Rescheduling` we call `StartSession`; from `Idle` we call
/// `ResumeSession`. From `Running` we do nothing. Any other status
/// (Terminated/Archived) is a hard error — the caller already
/// checked for those at step 1 so this should never fire, but we
/// keep the branch explicit.
async fn drive_to_running(temper: &TemperClient, session: &SessionRow) -> Result<()> {
    match session.status.as_str() {
        "Running" => Ok(()),
        "Rescheduling" => temper
            .invoke_session_action(&session.id, SessionAction::StartSession)
            .await
            .context("invoking StartSession"),
        "Idle" => temper
            .invoke_session_action(&session.id, SessionAction::ResumeSession)
            .await
            .context("invoking ResumeSession"),
        other => Err(anyhow!(
            "cannot drive session {} to Running from status {}",
            session.id,
            other
        )),
    }
}

/// Walk a chronologically sorted SessionEvent history and reduce it
/// to the `messages` array an Anthropic Messages-API request needs.
///
/// Rules:
/// - `user.message` → one user turn containing the extracted text
/// - `agent.message` → one assistant turn containing the extracted text
/// - every other kind is skipped (they are state pulses or
///   observability spans, not chat turns)
/// - malformed `Content` JSON surfaces as an `anyhow` error — we
///   refuse to silently drop turns because the model's reply would
///   then be wrong in a subtle way
///
/// This is `pub` so the unit tests at the bottom of the file can
/// exercise it directly.
pub fn events_to_messages(history: &[SessionEventRow]) -> Result<Vec<ChatMessage>> {
    let mut out = Vec::new();
    for ev in history {
        let role = match ev.kind.as_str() {
            "user.message" => "user",
            "agent.message" => "assistant",
            _ => continue,
        };
        let content = ev.content.as_deref().unwrap_or("");
        let text = extract_text_from_content_blob(content, &ev.kind, ev.sequence)?;
        out.push(ChatMessage {
            role: role.to_string(),
            content: vec![ContentBlock::text(text)],
        });
    }
    Ok(out)
}

/// Parse `{"blocks":[{"type":"text","text":"..."}]}` and return a
/// single concatenated text string. Tolerates extra unknown blocks
/// but ignores them (Phase 4 does not understand anything beyond
/// `text`). If the blob is not the expected shape, returns an error
/// so the caller can surface it rather than silently dropping the
/// turn.
fn extract_text_from_content_blob(blob: &str, kind: &str, sequence: i64) -> Result<String> {
    #[derive(Deserialize)]
    struct Envelope {
        blocks: Vec<Block>,
    }
    #[derive(Deserialize)]
    struct Block {
        #[serde(rename = "type")]
        ty: String,
        #[serde(default)]
        text: String,
    }

    if blob.trim().is_empty() {
        return Err(anyhow!(
            "{kind} event at sequence {sequence} has empty Content — cannot reconstruct chat history",
        ));
    }
    let env: Envelope = serde_json::from_str(blob).with_context(|| {
        format!(
            "decoding Content blob for {kind} event at sequence {sequence}: {blob}"
        )
    })?;
    let mut out = String::new();
    for b in env.blocks {
        if b.ty == "text" {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&b.text);
        }
    }
    if out.is_empty() {
        return Err(anyhow!(
            "{kind} event at sequence {sequence} has no text blocks in Content: {blob}",
        ));
    }
    Ok(out)
}

// ----------------------------------------------------------------------
// Helpers
// ----------------------------------------------------------------------

/// Build a deterministic event id. Idempotent across replays of the
/// same turn — if the caller re-invokes mid-failure, the second POST
/// will 409 on the duplicate primary key, which is the correct
/// resume signal.
fn event_id(session_id: &str, sequence: i64) -> String {
    format!("ev-auto-{session_id}-{sequence}")
}

/// Serialize a plain text string into the `{"blocks":[{"type":"text","text":"..."}]}`
/// shape the existing `SESSION_EVENTS_CURL_WALKTHROUGH.md` uses.
fn user_message_content(text: &str) -> String {
    serde_json::json!({
        "blocks": [{ "type": "text", "text": text }]
    })
    .to_string()
}

fn agent_message_content(text: &str) -> String {
    user_message_content(text)
}

/// A SessionEventRow with every optional column cleared. Used as
/// `..blank_event()` so the five kinds the responder emits each only
/// need to set the columns their field invariants actually require.
fn blank_event() -> SessionEventRow {
    SessionEventRow {
        id: String::new(),
        session_id: String::new(),
        sequence: 0,
        kind: String::new(),
        created_at: String::new(),
        processed_at: None,
        content: None,
        stop_reason: None,
        stop_reason_event_ids: None,
        model_request_start_id: None,
        is_error: None,
        model_input_tokens: None,
        model_output_tokens: None,
        model_cache_creation_input_tokens: None,
        model_cache_read_input_tokens: None,
        model_speed: None,
        tool_name: None,
        tool_use_id: None,
    }
}

/// Format a UTC timestamp as RFC-3339 with millisecond precision.
/// Kept as a tiny inline implementation to avoid pulling `chrono`
/// into the crate just for one format string.
fn now_rfc3339() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs() as i64;
    let millis = d.subsec_millis();
    // Convert epoch seconds to civil date via the standard trick.
    let (year, month, day, hour, minute, second) = epoch_to_civil(secs);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Convert a unix timestamp (seconds since 1970-01-01 UTC) to a
/// civil date tuple `(year, month, day, hour, minute, second)`.
/// Howard Hinnant's algorithm — no external calendar crate needed.
pub fn epoch_to_civil(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400) as u32;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day / 60) % 60;
    let second = secs_of_day % 60;
    (year, m, d, hour, minute, second)
}

/// Serialize helper used by the CLI for logging. Keeps the module
/// self-contained — no need to re-export serde types from other
/// modules.
#[derive(Serialize)]
pub struct TurnLog<'a> {
    pub session_id: &'a str,
    pub agent_id: &'a str,
    pub assistant_text: &'a str,
    pub input_tokens: i64,
    pub output_tokens: i64,
}

// ======================================================================
// Unit tests — pure-function coverage of events_to_messages +
// extract_text_from_content_blob + epoch_to_civil.
// ======================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn blank_row(seq: i64, kind: &str, content: Option<&str>) -> SessionEventRow {
        SessionEventRow {
            id: format!("ev-{seq}"),
            session_id: "sess-test".to_string(),
            sequence: seq,
            kind: kind.to_string(),
            created_at: "2026-04-11T00:00:00Z".to_string(),
            content: content.map(|s| s.to_string()),
            ..blank_event()
        }
    }

    fn text_blob(s: &str) -> String {
        serde_json::json!({ "blocks": [{ "type": "text", "text": s }] }).to_string()
    }

    #[test]
    fn empty_history_produces_empty_message_list() {
        let msgs = events_to_messages(&[]).unwrap();
        assert!(msgs.is_empty());
    }

    #[test]
    fn single_user_message_maps_to_single_user_turn() {
        let blob = text_blob("hello");
        let history = vec![blank_row(0, "user.message", Some(&blob))];
        let msgs = events_to_messages(&history).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[0].content[0].as_text(), "hello");
    }

    #[test]
    fn user_agent_user_produces_three_turns_in_order() {
        let history = vec![
            blank_row(0, "user.message", Some(&text_blob("q1"))),
            blank_row(1, "agent.message", Some(&text_blob("a1"))),
            blank_row(2, "user.message", Some(&text_blob("q2"))),
        ];
        let msgs = events_to_messages(&history).unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[0].content[0].as_text(), "q1");
        assert_eq!(msgs[1].role, "assistant");
        assert_eq!(msgs[1].content[0].as_text(), "a1");
        assert_eq!(msgs[2].role, "user");
        assert_eq!(msgs[2].content[0].as_text(), "q2");
    }

    #[test]
    fn non_chat_kinds_are_skipped() {
        let history = vec![
            blank_row(0, "session.status_running", Some("{}")),
            blank_row(1, "user.message", Some(&text_blob("q1"))),
            blank_row(2, "span.model_request_start", Some("{}")),
            blank_row(3, "agent.thinking", Some("{}")),
            blank_row(4, "agent.message", Some(&text_blob("a1"))),
            blank_row(5, "span.model_request_end", Some("{}")),
            blank_row(6, "session.status_idle", Some("{}")),
        ];
        let msgs = events_to_messages(&history).unwrap();
        assert_eq!(msgs.len(), 2, "only the two chat turns should survive");
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[0].content[0].as_text(), "q1");
        assert_eq!(msgs[1].role, "assistant");
        assert_eq!(msgs[1].content[0].as_text(), "a1");
    }

    #[test]
    fn malformed_content_blob_surfaces_as_error() {
        let history = vec![blank_row(0, "user.message", Some("not json"))];
        let err = events_to_messages(&history).unwrap_err();
        let s = err.to_string();
        assert!(
            s.contains("user.message") || s.contains("Content"),
            "error should mention the kind or Content: {s}"
        );
    }

    #[test]
    fn empty_content_blob_is_rejected() {
        let history = vec![blank_row(0, "user.message", Some(""))];
        let err = events_to_messages(&history).unwrap_err();
        assert!(
            err.to_string().contains("empty Content"),
            "{err}"
        );
    }

    #[test]
    fn content_with_no_text_blocks_is_rejected() {
        // An envelope whose `blocks` array is present but contains no
        // `text`-typed entries should be rejected — we cannot
        // reconstruct a chat turn from it.
        let blob = serde_json::json!({
            "blocks": [{ "type": "tool_use", "id": "x" }]
        })
        .to_string();
        let history = vec![blank_row(0, "agent.message", Some(&blob))];
        let err = events_to_messages(&history).unwrap_err();
        assert!(err.to_string().contains("no text blocks"), "{err}");
    }

    #[test]
    fn epoch_to_civil_matches_known_dates() {
        // 1970-01-01T00:00:00Z — unix epoch
        assert_eq!(epoch_to_civil(0), (1970, 1, 1, 0, 0, 0));
        // 1970-01-02T00:00:00Z — one day after epoch
        assert_eq!(epoch_to_civil(86_400), (1970, 1, 2, 0, 0, 0));
        // 2024-01-01T00:00:00Z — well-known reference timestamp
        assert_eq!(epoch_to_civil(1_704_067_200), (2024, 1, 1, 0, 0, 0));
        // 2024-02-29T00:00:00Z — leap-day edge case
        assert_eq!(epoch_to_civil(1_709_164_800), (2024, 2, 29, 0, 0, 0));
    }

    #[test]
    fn event_id_is_deterministic_per_session_and_sequence() {
        assert_eq!(event_id("sess-xyz", 0), "ev-auto-sess-xyz-0");
        assert_eq!(event_id("sess-xyz", 42), "ev-auto-sess-xyz-42");
    }

    #[test]
    fn user_message_content_wraps_text_in_blocks_envelope() {
        let out = user_message_content("hello world");
        // Parse it back — that is the only stable assertion about the
        // shape since serde_json does not guarantee key order.
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["blocks"][0]["type"], "text");
        assert_eq!(v["blocks"][0]["text"], "hello world");
    }
}
