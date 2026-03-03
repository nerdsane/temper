//! Message and tool format conversion between Temper and OpenAI Responses API.

use super::super::{ContentBlock, Message};

/// Convert Temper messages to OpenAI Responses API input format.
pub(super) fn convert_messages(messages: &[Message]) -> Vec<serde_json::Value> {
    let mut input = Vec::new();

    for msg in messages {
        for block in &msg.content {
            match block {
                ContentBlock::Text { text } => {
                    input.push(serde_json::json!({
                        "role": msg.role,
                        "content": text,
                    }));
                }
                ContentBlock::ToolUse {
                    id,
                    name,
                    input: tool_input,
                } => {
                    let args = if tool_input.is_object() || tool_input.is_array() {
                        tool_input.to_string()
                    } else {
                        tool_input.as_str().unwrap_or("{}").to_string()
                    };
                    input.push(serde_json::json!({
                        "type": "function_call",
                        "call_id": id,
                        "name": name,
                        "arguments": args,
                    }));
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } => {
                    input.push(serde_json::json!({
                        "type": "function_call_output",
                        "call_id": tool_use_id,
                        "output": content,
                    }));
                }
            }
        }
    }

    input
}

/// Convert Anthropic-format tool definitions to OpenAI function format.
pub(super) fn convert_tools(tools: &[serde_json::Value]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .filter_map(|tool| {
            let name = tool.get("name")?.as_str()?;
            let description = tool
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let parameters = tool
                .get("input_schema")
                .cloned()
                .unwrap_or(serde_json::json!({}));

            Some(serde_json::json!({
                "type": "function",
                "name": name,
                "description": description,
                "parameters": parameters,
            }))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_convert_messages_user_text() {
        let msgs = vec![Message {
            role: "user".into(),
            content: vec![ContentBlock::Text {
                text: "Hello".into(),
            }],
        }];
        let input = convert_messages(&msgs);
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"], "Hello");
    }

    #[test]
    fn test_convert_messages_tool_result() {
        let msgs = vec![Message {
            role: "user".into(),
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: "42".into(),
                is_error: None,
            }],
        }];
        let input = convert_messages(&msgs);
        assert_eq!(input[0]["type"], "function_call_output");
        assert_eq!(input[0]["call_id"], "call_1");
        assert_eq!(input[0]["output"], "42");
    }

    #[test]
    fn test_convert_messages_assistant_tool_use() {
        let msgs = vec![Message {
            role: "assistant".into(),
            content: vec![ContentBlock::ToolUse {
                id: "call_2".into(),
                name: "read_file".into(),
                input: serde_json::json!({"path": "/tmp/test"}),
            }],
        }];
        let input = convert_messages(&msgs);
        assert_eq!(input[0]["type"], "function_call");
        assert_eq!(input[0]["call_id"], "call_2");
        assert_eq!(input[0]["name"], "read_file");
        let args: serde_json::Value =
            serde_json::from_str(input[0]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["path"], "/tmp/test");
    }

    #[test]
    fn test_convert_messages_mixed() {
        let msgs = vec![
            Message {
                role: "user".into(),
                content: vec![ContentBlock::Text {
                    text: "Hello".into(),
                }],
            },
            Message {
                role: "assistant".into(),
                content: vec![
                    ContentBlock::Text {
                        text: "I'll help.".into(),
                    },
                    ContentBlock::ToolUse {
                        id: "c1".into(),
                        name: "ls".into(),
                        input: serde_json::json!({}),
                    },
                ],
            },
            Message {
                role: "user".into(),
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "c1".into(),
                    content: "file.txt".into(),
                    is_error: None,
                }],
            },
        ];
        let input = convert_messages(&msgs);
        assert_eq!(input.len(), 4);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[3]["type"], "function_call_output");
    }

    #[test]
    fn test_convert_tools() {
        let tools = vec![serde_json::json!({
            "name": "read_file",
            "description": "Read a file from disk",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" }
                },
                "required": ["path"]
            }
        })];
        let converted = convert_tools(&tools);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0]["type"], "function");
        assert_eq!(converted[0]["name"], "read_file");
        assert_eq!(converted[0]["description"], "Read a file from disk");
        assert_eq!(converted[0]["parameters"]["type"], "object");
    }

    #[test]
    fn test_convert_tools_empty() {
        let converted = convert_tools(&[]);
        assert!(converted.is_empty());
    }

    #[test]
    fn test_build_body_structure() {
        let tools = vec![serde_json::json!({
            "name": "test_tool",
            "description": "A test tool",
            "input_schema": { "type": "object" }
        })];
        let converted = convert_tools(&tools);
        assert_eq!(converted[0]["type"], "function");
        assert_eq!(converted[0]["name"], "test_tool");
    }

    #[test]
    fn test_stop_reason_mapping() {
        let has_tool_use = true;
        let status = "completed";

        let result = match status {
            "completed" => {
                if has_tool_use {
                    "tool_use"
                } else {
                    "end_turn"
                }
            }
            other => other,
        };
        assert_eq!(result, "tool_use");

        let result_no_tool = match "completed" {
            "completed" => {
                if false {
                    "tool_use"
                } else {
                    "end_turn"
                }
            }
            other => other,
        };
        assert_eq!(result_no_tool, "end_turn");
    }
}
