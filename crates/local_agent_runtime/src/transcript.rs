//! Transcript envelope stored in `Message.server_message_data` so tool calls and their
//! results round-trip through Warp's persisted conversation history losslessly.

use serde::{Deserialize, Serialize};

use crate::tools::{ToolCall, ToolCallResult};

const LOCAL_RUNTIME_TRANSCRIPT_VERSION: u8 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct LocalRuntimeTranscriptData {
    version: u8,
    message: LocalRuntimeTranscriptMessage,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum LocalRuntimeTranscriptMessage {
    ToolCall {
        call: ToolCall,
    },
    ToolResult {
        call_id: String,
        result: ToolCallResult,
    },
}

pub fn encode_local_runtime_tool_call_data(call: &ToolCall) -> String {
    serde_json::to_string(&LocalRuntimeTranscriptData {
        version: LOCAL_RUNTIME_TRANSCRIPT_VERSION,
        message: LocalRuntimeTranscriptMessage::ToolCall { call: call.clone() },
    })
    .unwrap_or_default()
}

pub fn encode_local_runtime_tool_result_data(
    call_id: impl Into<String>,
    result: &ToolCallResult,
) -> String {
    serde_json::to_string(&LocalRuntimeTranscriptData {
        version: LOCAL_RUNTIME_TRANSCRIPT_VERSION,
        message: LocalRuntimeTranscriptMessage::ToolResult {
            call_id: call_id.into(),
            result: result.clone(),
        },
    })
    .unwrap_or_default()
}

pub fn decode_local_runtime_tool_call_data(data: &str) -> Option<ToolCall> {
    let data: LocalRuntimeTranscriptData = serde_json::from_str(data).ok()?;
    if data.version != LOCAL_RUNTIME_TRANSCRIPT_VERSION {
        return None;
    }
    match data.message {
        LocalRuntimeTranscriptMessage::ToolCall { call } => Some(call),
        LocalRuntimeTranscriptMessage::ToolResult { .. } => None,
    }
}

pub fn decode_local_runtime_tool_result_data(data: &str) -> Option<(String, ToolCallResult)> {
    let data: LocalRuntimeTranscriptData = serde_json::from_str(data).ok()?;
    if data.version != LOCAL_RUNTIME_TRANSCRIPT_VERSION {
        return None;
    }
    match data.message {
        LocalRuntimeTranscriptMessage::ToolResult { call_id, result } => Some((call_id, result)),
        LocalRuntimeTranscriptMessage::ToolCall { .. } => None,
    }
}

#[cfg(test)]
#[path = "transcript_tests.rs"]
mod tests;
