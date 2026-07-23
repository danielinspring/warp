use super::*;
use crate::messages::{AssistantMessage, SystemMessage, ToolResultMessage, UserMessage};
use crate::tools::{ToolCall, ToolCallResult};

fn small_budget(max_input_tokens: usize) -> ContextBudget {
    ContextBudget {
        max_input_tokens: Some(max_input_tokens),
        reserved_output_tokens: 0,
        approximate_chars_per_token: 1,
        max_tool_result_chars: 10_000,
        preserve_recent_turns: 2,
        max_compaction_summary_chars: 300,
    }
}

#[test]
fn compaction_preserves_recent_turns_and_never_splits_tool_pairs() {
    let messages = vec![
        Message::System(SystemMessage {
            content: "system".to_string(),
        }),
        Message::User(UserMessage {
            content: "old request ".repeat(30),
        }),
        Message::Assistant(AssistantMessage {
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: "call_1".to_string(),
                name: "read_files".to_string(),
                arguments: serde_json::json!({ "paths": ["large.rs"] }),
            }],
        }),
        Message::ToolResult(ToolResultMessage {
            call_id: "call_1".to_string(),
            result: ToolCallResult::success("old result ".repeat(30)),
        }),
        Message::User(UserMessage {
            content: "recent request".to_string(),
        }),
        Message::Assistant(AssistantMessage {
            content: "recent response".to_string(),
            tool_calls: vec![],
        }),
        Message::User(UserMessage {
            content: "current request".to_string(),
        }),
    ];
    let original = messages.clone();

    let model = model_messages(&messages, &[], &small_budget(420)).unwrap();

    assert!(matches!(&model[0], Message::System(message) if message.content == "system"));
    assert!(model.iter().any(
        |message| matches!(message, Message::System(message) if message.content.contains("\"compacted\":true"))
    ));
    assert!(!model.iter().any(|message| {
        matches!(message, Message::Assistant(message) if message.tool_calls.iter().any(|call| call.id == "call_1"))
            || matches!(message, Message::ToolResult(message) if message.call_id == "call_1")
    }));
    assert!(model.iter().any(
        |message| matches!(message, Message::User(message) if message.content == "recent request")
    ));
    assert!(model.iter().any(
        |message| matches!(message, Message::User(message) if message.content == "current request")
    ));
    assert_eq!(message_chars(&messages), message_chars(&original));
}

#[test]
fn tool_schema_and_output_reserves_are_part_of_the_same_budget() {
    let tools = vec![ToolSchema {
        name: "large_tool".to_string(),
        description: "x".repeat(200),
        parameters: serde_json::json!({ "type": "object" }),
    }];
    let messages = vec![Message::User(UserMessage {
        content: "hello".to_string(),
    })];
    let mut budget = small_budget(100);
    budget.reserved_output_tokens = 25;

    let error = model_messages(&messages, &tools, &budget).unwrap_err();

    assert!(matches!(
        error,
        RuntimeError::ContextBudgetExceeded { limit: 100, .. }
    ));
}

#[test]
fn impossible_recent_turn_budget_returns_structured_error() {
    let messages = vec![
        Message::System(SystemMessage {
            content: "system".repeat(100),
        }),
        Message::User(UserMessage {
            content: "current".to_string(),
        }),
    ];

    let error = model_messages(&messages, &[], &small_budget(20)).unwrap_err();

    assert!(matches!(
        error,
        RuntimeError::ContextBudgetExceeded {
            estimated_tokens,
            limit: 20,
        } if estimated_tokens > 20
    ));
}
