use super::*;

#[test]
fn transcript_envelope_round_trips_exact_tool_data() {
    let call = ToolCall {
        id: "mcp_call_1".to_string(),
        name: "mcp__github__search".to_string(),
        arguments: serde_json::json!({
            "query": "local runtime",
            "nested": { "limit": 3 },
        }),
    };
    let encoded_call = encode_local_runtime_tool_call_data(&call);
    let decoded_call = decode_local_runtime_tool_call_data(&encoded_call).unwrap();
    assert_eq!(decoded_call.id, call.id);
    assert_eq!(decoded_call.name, call.name);
    assert_eq!(decoded_call.arguments, call.arguments);

    let result = ToolCallResult::error(r#"{"error":"denied"}"#);
    let encoded_result = encode_local_runtime_tool_result_data(call.id.clone(), &result);
    let (decoded_call_id, decoded_result) =
        decode_local_runtime_tool_result_data(&encoded_result).unwrap();
    assert_eq!(decoded_call_id, call.id);
    assert_eq!(decoded_result.content, result.content);
    assert!(decoded_result.is_error);
}

#[test]
fn malformed_or_wrong_kind_transcript_envelope_is_ignored() {
    assert!(decode_local_runtime_tool_call_data("{not json").is_none());
    let encoded_result =
        encode_local_runtime_tool_result_data("call_1", &ToolCallResult::success("ok"));
    assert!(decode_local_runtime_tool_call_data(&encoded_result).is_none());
    assert!(
        decode_local_runtime_tool_result_data(&encode_local_runtime_tool_call_data(&ToolCall {
            id: "call_1".to_string(),
            name: "read_files".to_string(),
            arguments: serde_json::json!({}),
        }))
        .is_none()
    );
}
