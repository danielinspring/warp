use local_agent_runtime::messages::{
    AssistantMessage, ContentPart, ToolResultMessage, UserMessage,
};

use super::*;
use crate::ai::agent::UserQueryMode;
use crate::ai::local_runtime_bridge::{
    encode_local_runtime_tool_call_data, encode_local_runtime_tool_result_data,
};

/// Builds a small valid PNG and returns it base64-encoded (standard alphabet).
fn test_png_base64() -> String {
    use base64::Engine as _;
    use image::{ImageBuffer, Rgba};

    let img: ImageBuffer<Rgba<u8>, Vec<u8>> =
        ImageBuffer::from_fn(10, 10, |_x, _y| Rgba([255u8, 0u8, 0u8, 255u8]));
    let mut bytes = Vec::new();
    img.write_to(
        &mut std::io::Cursor::new(&mut bytes),
        image::ImageFormat::Png,
    )
    .unwrap();
    base64::engine::general_purpose::STANDARD.encode(&bytes)
}

fn user_query_input(query: &str, context: Vec<AIAgentContext>) -> AIAgentInput {
    AIAgentInput::UserQuery {
        query: query.to_string(),
        context: context.into(),
        static_query_type: None,
        referenced_attachments: Default::default(),
        user_query_mode: UserQueryMode::default(),
        running_command: None,
        intended_agent: None,
    }
}

fn image_context(data: &str, mime_type: &str, file_name: &str) -> AIAgentContext {
    AIAgentContext::Image(ImageContext {
        data: data.to_string(),
        mime_type: mime_type.to_string(),
        file_name: file_name.to_string(),
        is_figma: false,
    })
}

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
        Message::User(UserMessage::text("keep")),
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
    assert!(matches!(&retained[0], Message::User(message) if message.text_content() == "keep"));
}

#[test]
fn extract_user_input_attaches_processed_image_as_content_part() {
    let png_base64 = test_png_base64();
    let mut params = RequestParams::new_for_test();
    params.input = vec![user_query_input(
        "what is in this screenshot?",
        vec![image_context(&png_base64, "image/png", "shot.png")],
    )];

    let user_message = extract_user_input(&params);

    assert_eq!(user_message.text_content(), "what is in this screenshot?");
    assert!(user_message.has_images());
    let image_part = user_message
        .parts
        .iter()
        .find(|part| matches!(part, ContentPart::Image { .. }))
        .expect("expected an image content part");
    let ContentPart::Image {
        mime_type,
        file_name,
        data_base64,
    } = image_part
    else {
        unreachable!("filtered to image parts above");
    };
    assert_eq!(mime_type, "image/png");
    assert_eq!(file_name.as_deref(), Some("shot.png"));
    assert!(!data_base64.is_empty());
}

#[test]
fn extract_user_input_is_text_only_when_no_images_are_attached() {
    let mut params = RequestParams::new_for_test();
    params.input = vec![user_query_input("just text, no images", vec![])];

    let user_message = extract_user_input(&params);

    assert_eq!(user_message.text_content(), "just text, no images");
    assert!(!user_message.has_images());
    assert_eq!(user_message.parts.len(), 1);
}

#[test]
fn extract_user_input_caps_images_at_max_query_count() {
    let png_base64 = test_png_base64();
    let mut params = RequestParams::new_for_test();
    let contexts = (0..(MAX_IMAGE_COUNT_FOR_QUERY + 5))
        .map(|index| image_context(&png_base64, "image/png", &format!("shot-{index}.png")))
        .collect();
    params.input = vec![user_query_input("many images", contexts)];

    let user_message = extract_user_input(&params);

    let image_count = user_message
        .parts
        .iter()
        .filter(|part| matches!(part, ContentPart::Image { .. }))
        .count();
    assert_eq!(image_count, MAX_IMAGE_COUNT_FOR_QUERY);
}

#[test]
fn extract_user_input_skips_unparseable_image_bytes() {
    let mut params = RequestParams::new_for_test();
    params.input = vec![user_query_input(
        "bad image",
        vec![image_context(
            "not-valid-base64-image-data",
            "image/png",
            "bad.png",
        )],
    )];

    let user_message = extract_user_input(&params);

    assert_eq!(user_message.text_content(), "bad image");
    assert!(!user_message.has_images());
}
