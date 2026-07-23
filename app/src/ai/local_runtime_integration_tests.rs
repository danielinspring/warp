use local_agent_runtime::messages::{AssistantMessage, ToolResultMessage, UserMessage};

use super::*;
use crate::ai::local_runtime_bridge::{
    encode_local_runtime_tool_call_data, encode_local_runtime_tool_result_data,
};

fn proto_message(message: api::message::Message, server_message_data: String) -> api::Message {
    api::Message {
        id: Uuid::new_v4().to_string(),
        task_id: "task_1".to_string(),
        request_id: "request_1".to_string(),
        timestamp: None,
        server_message_data,
        citations: vec![],
        fetched_memories: vec![],
        message: Some(message),
    }
}

#[test]
fn canonical_transcript_data_restores_exact_call_and_error_result() {
    let registry = LocalRuntimeToolRegistry::built_ins();
    let call = local_agent_runtime::ToolCall {
        id: "call_1".to_string(),
        name: "edit_files".to_string(),
        arguments: serde_json::json!({
            "title": "Update greeting",
            "edits": [{
                "type": "replace",
                "file": "hello.rs",
                "search": "Hello",
                "replace": "Hello, Warp",
            }],
        }),
    };
    let result = local_agent_runtime::ToolCallResult::error(r#"{"status":"cancelled"}"#);
    let tool_call = proto_message(
        api::message::Message::ToolCall(api::message::ToolCall {
            tool_call_id: call.id.clone(),
            tool: Some(api::message::tool_call::Tool::ApplyFileDiffs(
                api::message::tool_call::ApplyFileDiffs::default(),
            )),
        }),
        encode_local_runtime_tool_call_data(&call),
    );
    let tool_result = proto_message(
        api::message::Message::ToolCallResult(api::message::ToolCallResult {
            tool_call_id: call.id.clone(),
            context: None,
            result: Some(api::message::tool_call_result::Result::Cancel(())),
        }),
        encode_local_runtime_tool_result_data(call.id.clone(), &result),
    );

    let restored_call = translate_proto_to_runtime_message(&tool_call, &registry).unwrap();
    let restored_result = translate_proto_to_runtime_message(&tool_result, &registry).unwrap();

    let Message::Assistant(restored_call) = restored_call else {
        panic!("expected assistant tool call");
    };
    assert_eq!(restored_call.tool_calls[0].id, call.id);
    assert_eq!(restored_call.tool_calls[0].name, call.name);
    assert_eq!(restored_call.tool_calls[0].arguments, call.arguments);

    let Message::ToolResult(restored_result) = restored_result else {
        panic!("expected tool result");
    };
    assert_eq!(restored_result.call_id, call.id);
    assert_eq!(restored_result.result.content, result.content);
    assert!(restored_result.result.is_error);
}

#[test]
fn unpaired_tool_calls_and_results_are_removed_together() {
    let messages = vec![
        Message::User(UserMessage {
            content: "keep".to_string(),
        }),
        Message::Assistant(AssistantMessage {
            content: String::new(),
            tool_calls: vec![local_agent_runtime::ToolCall {
                id: "orphan_call".to_string(),
                name: "read_files".to_string(),
                arguments: serde_json::json!({ "paths": ["a.rs"] }),
            }],
        }),
        Message::ToolResult(ToolResultMessage {
            call_id: "orphan_result".to_string(),
            result: local_agent_runtime::ToolCallResult::success("unused"),
        }),
    ];

    let retained = retain_paired_tool_messages(messages);

    assert_eq!(retained.len(), 1);
    assert!(matches!(&retained[0], Message::User(message) if message.content == "keep"));
}
