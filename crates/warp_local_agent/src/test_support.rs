//! Protobuf request fixtures shared by the crate's tests.

use local_agent_runtime::{
    ToolCall, ToolCallResult, encode_local_runtime_tool_call_data,
    encode_local_runtime_tool_result_data,
};
use warp_multi_agent_api as api;
use warp_multi_agent_api::request::input::tool_call_result::Result as RequestResult;
use warp_multi_agent_api::request::input::user_inputs::UserInput;
use warp_multi_agent_api::request::input::user_inputs::user_input::Input;
use warp_multi_agent_api::request::settings::custom_model_providers::{
    CustomEndpointSchema, CustomModel, CustomModelProvider,
};
use warp_multi_agent_api::request::settings::{CustomModelProviders, ModelConfig};

pub(crate) const TEST_MODEL: &str = "qwen2.5-coder:7b";
pub(crate) const TEST_BASE_URL: &str = "http://127.0.0.1:11434";
pub(crate) const TEST_CONFIG_KEY: &str = "local-ollama";

/// Settings that route `model_config.base` to a local OpenAI-compatible provider.
pub(crate) fn settings() -> api::request::Settings {
    api::request::Settings {
        model_config: Some(ModelConfig {
            base: TEST_CONFIG_KEY.to_string(),
            ..Default::default()
        }),
        custom_model_providers: Some(CustomModelProviders {
            providers: vec![CustomModelProvider {
                base_url: TEST_BASE_URL.to_string(),
                api_key: String::new(),
                schema: CustomEndpointSchema::OpenaiChatCompletions as i32,
                models: vec![CustomModel {
                    slug: TEST_MODEL.to_string(),
                    config_key: TEST_CONFIG_KEY.to_string(),
                    reasoning_effort: String::new(),
                }],
            }],
        }),
        ..Default::default()
    }
}

pub(crate) fn plan_mode() -> api::UserQueryMode {
    api::UserQueryMode {
        r#type: Some(api::user_query_mode::Type::Plan(())),
    }
}

pub(crate) fn user_query(query: &str, mode: Option<api::UserQueryMode>) -> UserInput {
    UserInput {
        input: Some(Input::UserQuery(api::request::input::UserQuery {
            query: query.to_string(),
            mode,
            ..Default::default()
        })),
    }
}

pub(crate) fn tool_call_result(tool_call_id: &str, result: Option<RequestResult>) -> UserInput {
    UserInput {
        input: Some(Input::ToolCallResult(api::request::input::ToolCallResult {
            tool_call_id: tool_call_id.to_string(),
            result,
        })),
    }
}

pub(crate) fn input(inputs: Vec<UserInput>) -> api::request::Input {
    api::request::Input {
        context: Some(api::InputContext::default()),
        r#type: Some(api::request::input::Type::UserInputs(
            api::request::input::UserInputs { inputs },
        )),
    }
}

/// A request carrying `inputs`, the test provider settings and a fixed conversation id.
pub(crate) fn request(inputs: Vec<UserInput>) -> api::Request {
    api::Request {
        input: Some(input(inputs)),
        settings: Some(settings()),
        metadata: Some(api::request::Metadata {
            conversation_id: "conversation_1".to_string(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

pub(crate) fn query_request(query: &str) -> api::Request {
    request(vec![user_query(query, None)])
}

pub(crate) fn task(id: &str, messages: Vec<api::Message>) -> api::Task {
    api::Task {
        id: id.to_string(),
        messages,
        ..Default::default()
    }
}

fn message(
    task_id: &str,
    server_message_data: String,
    message: api::message::Message,
) -> api::Message {
    api::Message {
        id: uuid::Uuid::new_v4().to_string(),
        task_id: task_id.to_string(),
        request_id: "request_0".to_string(),
        timestamp: None,
        server_message_data,
        citations: vec![],
        fetched_memories: vec![],
        message: Some(message),
    }
}

pub(crate) fn user_query_message(
    task_id: &str,
    query: &str,
    mode: Option<api::UserQueryMode>,
) -> api::Message {
    message(
        task_id,
        String::new(),
        api::message::Message::UserQuery(api::message::UserQuery {
            query: query.to_string(),
            mode,
            ..Default::default()
        }),
    )
}

pub(crate) fn agent_output_message(task_id: &str, text: &str) -> api::Message {
    message(
        task_id,
        String::new(),
        api::message::Message::AgentOutput(api::message::AgentOutput {
            text: text.to_string(),
        }),
    )
}

/// A persisted tool call carrying the transcript envelope, as the service emits it.
pub(crate) fn tool_call_message(
    task_id: &str,
    call: &ToolCall,
    tool: Option<api::message::tool_call::Tool>,
) -> api::Message {
    message(
        task_id,
        encode_local_runtime_tool_call_data(call),
        api::message::Message::ToolCall(api::message::ToolCall {
            tool_call_id: call.id.clone(),
            tool,
        }),
    )
}

/// A persisted tool result carrying the transcript envelope, as the service emits it.
pub(crate) fn tool_call_result_message(
    task_id: &str,
    call_id: &str,
    result: &ToolCallResult,
) -> api::Message {
    message(
        task_id,
        encode_local_runtime_tool_result_data(call_id, result),
        api::message::Message::ToolCallResult(api::message::ToolCallResult {
            tool_call_id: call_id.to_string(),
            context: None,
            result: None,
        }),
    )
}

pub(crate) fn shell_call(id: &str, command: &str) -> ToolCall {
    ToolCall {
        id: id.to_string(),
        name: "run_shell_command".to_string(),
        arguments: serde_json::json!({ "command": command }),
    }
}

/// A finished `run_shell_command` result with the given exit code and output.
pub(crate) fn shell_result(command: &str, output: &str, exit_code: i32) -> RequestResult {
    RequestResult::RunShellCommand(api::RunShellCommandResult {
        command: command.to_string(),
        result: Some(api::run_shell_command_result::Result::CommandFinished(
            api::ShellCommandFinished {
                exit_code,
                output: output.to_string(),
                ..Default::default()
            },
        )),
        ..Default::default()
    })
}
