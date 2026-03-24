//! Session Tree Library — shared JSONL tree operations for TemperAgent WASM modules.
//!
//! Provides append-only tree-structured conversation storage with branching,
//! compaction support, and leaf-to-root context assembly.
//!
//! Storage format: JSONL (one JSON object per line) with tree structure via id/parentId.

use std::collections::BTreeMap;
use serde_json::{Value, json};

/// A single entry in the session tree.
#[derive(Debug, Clone)]
pub struct SessionEntry {
    pub id: String,
    pub parent_id: Option<String>,
    pub entry_type: EntryType,
    pub data: Value,
    pub tokens: usize,
}

/// Type of session tree entry.
#[derive(Debug, Clone, PartialEq)]
pub enum EntryType {
    /// Session header with metadata.
    Header,
    /// A conversation message (user, assistant, or tool_result).
    Message,
    /// A compaction summary replacing older messages.
    Compaction,
    /// A steering injection point.
    Steering,
}

impl EntryType {
    pub fn as_str(&self) -> &str {
        match self {
            EntryType::Header => "header",
            EntryType::Message => "message",
            EntryType::Compaction => "compaction",
            EntryType::Steering => "steering",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "header" => EntryType::Header,
            "message" => EntryType::Message,
            "compaction" => EntryType::Compaction,
            "steering" => EntryType::Steering,
            _ => EntryType::Message,
        }
    }
}

/// The session tree — an append-only tree of conversation entries.
pub struct SessionTree {
    entries: BTreeMap<String, SessionEntry>,
    /// Ordered list of entry IDs (insertion order).
    order: Vec<String>,
    /// Raw JSONL lines for serialization.
    raw_lines: Vec<String>,
}

impl SessionTree {
    /// Parse a JSONL string into a SessionTree.
    pub fn from_jsonl(data: &str) -> Self {
        let mut entries = BTreeMap::new();
        let mut order = Vec::new();
        let mut raw_lines = Vec::new();

        for line in data.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            raw_lines.push(line.to_string());

            if let Ok(val) = serde_json::from_str::<Value>(line) {
                let id = val.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let parent_id = val.get("parentId").and_then(|v| v.as_str()).map(|s| s.to_string());
                let entry_type = val.get("type").and_then(|v| v.as_str()).map(EntryType::from_str).unwrap_or(EntryType::Message);
                let tokens = val.get("tokens").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

                if !id.is_empty() {
                    let entry = SessionEntry {
                        id: id.clone(),
                        parent_id,
                        entry_type,
                        data: val,
                        tokens,
                    };
                    order.push(id.clone());
                    entries.insert(id, entry);
                }
            }
        }

        SessionTree { entries, order, raw_lines }
    }

    /// Create an empty session tree with a header entry.
    pub fn new(session_id: &str) -> Self {
        let header = json!({
            "id": format!("h-{session_id}"),
            "parentId": null,
            "type": "header",
            "version": 1,
            "created": "",
            "tokens": 0
        });
        let header_line = serde_json::to_string(&header).unwrap_or_default();
        let id = format!("h-{session_id}");

        let entry = SessionEntry {
            id: id.clone(),
            parent_id: None,
            entry_type: EntryType::Header,
            data: header,
            tokens: 0,
        };

        let mut entries = BTreeMap::new();
        entries.insert(id.clone(), entry);

        SessionTree {
            entries,
            order: vec![id],
            raw_lines: vec![header_line],
        }
    }

    /// Check if the tree is empty (no entries at all).
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Get the number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Get an entry by ID.
    pub fn get(&self, id: &str) -> Option<&SessionEntry> {
        self.entries.get(id)
    }

    /// Find the last entry ID (the most recently appended).
    pub fn last_entry_id(&self) -> Option<&str> {
        self.order.last().map(|s| s.as_str())
    }

    /// Build context messages by walking from leaf_id to root.
    /// Handles compaction entries: when a compaction is encountered,
    /// it replaces all entries before it with the summary.
    pub fn build_context(&self, leaf_id: &str) -> Vec<Value> {
        // Walk from leaf to root collecting entries
        let mut chain: Vec<&SessionEntry> = Vec::new();
        let mut current_id = Some(leaf_id.to_string());

        while let Some(id) = current_id {
            if let Some(entry) = self.entries.get(&id) {
                chain.push(entry);
                current_id = entry.parent_id.clone();
            } else {
                break;
            }
        }

        // Reverse to get root-to-leaf order
        chain.reverse();

        // Build messages, handling compaction entries
        let mut messages: Vec<Value> = Vec::new();

        for entry in &chain {
            match entry.entry_type {
                EntryType::Header => {
                    // Skip headers — they're metadata
                    continue;
                }
                EntryType::Compaction => {
                    // A compaction replaces all prior messages with its summary
                    messages.clear();
                    if let Some(summary) = entry.data.get("summary").and_then(|v| v.as_str()) {
                        messages.push(json!({
                            "role": "user",
                            "content": format!("[Previous conversation summary]\n{summary}")
                        }));
                    }
                }
                EntryType::Message | EntryType::Steering => {
                    // Extract role and content from the entry
                    let role = entry.data.get("role").and_then(|v| v.as_str()).unwrap_or("user");
                    if let Some(content) = entry.data.get("content").cloned() {
                        messages.push(json!({
                            "role": role,
                            "content": content,
                        }));
                    }
                }
            }
        }

        messages
    }

    /// Append a new entry to the tree. Returns the JSONL line for the new entry.
    /// The entry is added with the given parent_id.
    pub fn append_entry(
        &mut self,
        id: &str,
        parent_id: Option<&str>,
        entry_type: EntryType,
        role: Option<&str>,
        content: Option<&Value>,
        tokens: usize,
        extra_fields: Option<&Value>,
    ) -> String {
        let mut data = json!({
            "id": id,
            "parentId": parent_id,
            "type": entry_type.as_str(),
            "tokens": tokens,
        });

        if let Some(role) = role {
            data["role"] = json!(role);
        }
        if let Some(content) = content {
            data["content"] = content.clone();
        }
        if let Some(extra) = extra_fields {
            if let Some(obj) = extra.as_object() {
                for (k, v) in obj {
                    data[k] = v.clone();
                }
            }
        }

        let line = serde_json::to_string(&data).unwrap_or_default();

        let entry = SessionEntry {
            id: id.to_string(),
            parent_id: parent_id.map(|s| s.to_string()),
            entry_type,
            data,
            tokens,
        };

        self.order.push(id.to_string());
        self.entries.insert(id.to_string(), entry);
        self.raw_lines.push(line.clone());

        line
    }

    /// Append a user message. Returns (entry_id, jsonl_line).
    pub fn append_user_message(&mut self, parent_id: &str, content: &str, tokens: usize) -> (String, String) {
        let id = format!("u-{}", self.order.len());
        let line = self.append_entry(
            &id,
            Some(parent_id),
            EntryType::Message,
            Some("user"),
            Some(&json!(content)),
            tokens,
            None,
        );
        (id, line)
    }

    /// Append an assistant message. Returns (entry_id, jsonl_line).
    pub fn append_assistant_message(&mut self, parent_id: &str, content: &Value, tokens: usize) -> (String, String) {
        let id = format!("a-{}", self.order.len());
        let line = self.append_entry(
            &id,
            Some(parent_id),
            EntryType::Message,
            Some("assistant"),
            Some(content),
            tokens,
            None,
        );
        (id, line)
    }

    /// Append a tool result message (role: user with tool_result content). Returns (entry_id, jsonl_line).
    pub fn append_tool_results(&mut self, parent_id: &str, tool_results: &Value, tokens: usize) -> (String, String) {
        let id = format!("t-{}", self.order.len());
        let line = self.append_entry(
            &id,
            Some(parent_id),
            EntryType::Message,
            Some("user"),
            Some(tool_results),
            tokens,
            None,
        );
        (id, line)
    }

    /// Append a compaction entry. Returns (entry_id, jsonl_line).
    pub fn append_compaction(&mut self, parent_id: &str, summary: &str, first_kept: &str) -> (String, String) {
        let id = format!("c-{}", self.order.len());
        let extra = json!({
            "summary": summary,
            "first_kept": first_kept,
        });
        let line = self.append_entry(
            &id,
            Some(parent_id),
            EntryType::Compaction,
            None,
            None,
            0,
            Some(&extra),
        );
        (id, line)
    }

    /// Append a steering message. Returns (entry_id, jsonl_line).
    pub fn append_steering_message(&mut self, parent_id: &str, content: &str, tokens: usize) -> (String, String) {
        let id = format!("s-{}", self.order.len());
        let line = self.append_entry(
            &id,
            Some(parent_id),
            EntryType::Steering,
            Some("user"),
            Some(&json!(content)),
            tokens,
            None,
        );
        (id, line)
    }

    /// Estimate total tokens in the context for a given leaf.
    pub fn estimate_tokens(&self, leaf_id: &str) -> usize {
        let mut total = 0;
        let mut current_id = Some(leaf_id.to_string());

        while let Some(id) = current_id {
            if let Some(entry) = self.entries.get(&id) {
                if entry.entry_type == EntryType::Compaction {
                    // After compaction, only count from here forward
                    total += entry.tokens;
                    break;
                }
                total += entry.tokens;
                current_id = entry.parent_id.clone();
            } else {
                break;
            }
        }

        total
    }

    /// Find a cut point for compaction. Returns the entry ID where we should
    /// start keeping messages (everything before this gets compacted).
    /// Walks backward from the leaf keeping `keep_recent_tokens` worth of messages.
    pub fn find_cut_point(&self, leaf_id: &str, keep_recent_tokens: usize) -> Option<String> {
        let mut accumulated = 0;
        let mut current_id = Some(leaf_id.to_string());
        let mut cut_point = None;

        while let Some(id) = current_id {
            if let Some(entry) = self.entries.get(&id) {
                accumulated += entry.tokens;
                if accumulated >= keep_recent_tokens {
                    // This is where we should cut — keep everything after this
                    cut_point = Some(id.clone());
                    break;
                }
                current_id = entry.parent_id.clone();
            } else {
                break;
            }
        }

        cut_point
    }

    /// Serialize the tree back to JSONL format.
    pub fn to_jsonl(&self) -> String {
        self.raw_lines.join("\n")
    }

    /// Get all entry IDs in insertion order.
    pub fn entry_ids(&self) -> &[String] {
        &self.order
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_session_tree() {
        let tree = SessionTree::new("test-1");
        assert_eq!(tree.len(), 1);
        assert!(!tree.is_empty());
    }

    #[test]
    fn test_append_and_build_context() {
        let mut tree = SessionTree::new("test-1");
        let header_id = tree.last_entry_id().unwrap().to_string();

        let (user_id, _) = tree.append_user_message(&header_id, "Hello", 10);
        let (asst_id, _) = tree.append_assistant_message(&user_id, &json!([{"type": "text", "text": "Hi there!"}]), 20);

        let messages = tree.build_context(&asst_id);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[1]["role"], "assistant");
    }

    #[test]
    fn test_compaction() {
        let mut tree = SessionTree::new("test-1");
        let header_id = tree.last_entry_id().unwrap().to_string();

        let (u1, _) = tree.append_user_message(&header_id, "First message", 100);
        let (a1, _) = tree.append_assistant_message(&u1, &json!("Response 1"), 200);
        let (compact_id, _) = tree.append_compaction(&a1, "Summary of conversation so far", &a1);
        let (u2, _) = tree.append_user_message(&compact_id, "New message after compaction", 50);

        let messages = tree.build_context(&u2);
        // Should have: compaction summary + new message
        assert_eq!(messages.len(), 2);
        assert!(messages[0]["content"].as_str().unwrap().contains("summary"));
    }

    #[test]
    fn test_from_jsonl() {
        let jsonl = r#"{"id":"h-1","parentId":null,"type":"header","version":1,"tokens":0}
{"id":"u-1","parentId":"h-1","type":"message","role":"user","content":"Hello","tokens":10}
{"id":"a-1","parentId":"u-1","type":"message","role":"assistant","content":"Hi!","tokens":5}"#;

        let tree = SessionTree::from_jsonl(jsonl);
        assert_eq!(tree.len(), 3);

        let messages = tree.build_context("a-1");
        assert_eq!(messages.len(), 2);
    }

    #[test]
    fn test_to_jsonl_roundtrip() {
        let mut tree = SessionTree::new("test-1");
        let header_id = tree.last_entry_id().unwrap().to_string();
        tree.append_user_message(&header_id, "Hello", 10);

        let jsonl = tree.to_jsonl();
        let tree2 = SessionTree::from_jsonl(&jsonl);
        assert_eq!(tree2.len(), tree.len());
    }

    #[test]
    fn test_estimate_tokens() {
        let mut tree = SessionTree::new("test-1");
        let header_id = tree.last_entry_id().unwrap().to_string();

        let (u1, _) = tree.append_user_message(&header_id, "Hello", 100);
        let (a1, _) = tree.append_assistant_message(&u1, &json!("Response"), 200);

        assert_eq!(tree.estimate_tokens(&a1), 300);
    }

    #[test]
    fn test_find_cut_point() {
        let mut tree = SessionTree::new("test-1");
        let header_id = tree.last_entry_id().unwrap().to_string();

        let (u1, _) = tree.append_user_message(&header_id, "Msg 1", 100);
        let (a1, _) = tree.append_assistant_message(&u1, &json!("Resp 1"), 200);
        let (u2, _) = tree.append_user_message(&a1, "Msg 2", 100);
        let (a2, _) = tree.append_assistant_message(&u2, &json!("Resp 2"), 200);

        // Keep 250 tokens — should cut somewhere in the middle
        let cut = tree.find_cut_point(&a2, 250);
        assert!(cut.is_some());
    }
}
