//! Drives a running local agent service the way the Warp client does, without Warp.
//!
//! Start the service, then point this at any OpenAI-compatible endpoint:
//!
//! ```sh
//! cargo run -p warp_local_agent -- --listen 127.0.0.1:9377 &
//! LOCAL_AGENT_PROVIDER_URL=http://host:4000/v1 \
//! LOCAL_AGENT_PROVIDER_KEY=sk-... \
//! LOCAL_AGENT_MODEL=qwen3-coder:latest \
//!   cargo run -p warp_local_agent --example smoke -- "what is 2+2?"
//! ```
//!
//! Credentials are read from the environment so they stay out of the repository.

use base64::Engine as _;
use base64::prelude::BASE64_URL_SAFE;
use prost::Message as _;
use warp_multi_agent_api as api;

/// Ties `model_config.base` to the provider entry, exactly as the Warp client does.
const CONFIG_KEY: &str = "local-ollama";

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn build_request(prompt: &str, provider_url: &str, api_key: &str, model: &str) -> api::Request {
    use api::request::input::user_inputs::UserInput;
    use api::request::input::user_inputs::user_input::Input;
    use api::request::settings::custom_model_providers::{
        CustomEndpointSchema, CustomModel, CustomModelProvider,
    };

    api::Request {
        input: Some(api::request::Input {
            context: Some(api::InputContext {
                directory: Some(api::input_context::Directory {
                    pwd: std::env::current_dir()
                        .map(|path| path.display().to_string())
                        .unwrap_or_default(),
                    ..Default::default()
                }),
                shell: Some(api::input_context::Shell {
                    name: "zsh".to_string(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            r#type: Some(api::request::input::Type::UserInputs(
                api::request::input::UserInputs {
                    inputs: vec![UserInput {
                        input: Some(Input::UserQuery(api::request::input::UserQuery {
                            query: prompt.to_string(),
                            ..Default::default()
                        })),
                    }],
                },
            )),
        }),
        settings: Some(api::request::Settings {
            model_config: Some(api::request::settings::ModelConfig {
                base: CONFIG_KEY.to_string(),
                ..Default::default()
            }),
            custom_model_providers: Some(api::request::settings::CustomModelProviders {
                providers: vec![CustomModelProvider {
                    base_url: provider_url.to_string(),
                    api_key: api_key.to_string(),
                    schema: CustomEndpointSchema::OpenaiChatCompletions as i32,
                    models: vec![CustomModel {
                        slug: model.to_string(),
                        config_key: CONFIG_KEY.to_string(),
                        reasoning_effort: String::new(),
                    }],
                }],
            }),
            ..Default::default()
        }),
        metadata: Some(api::request::Metadata {
            conversation_id: uuid::Uuid::new_v4().to_string(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn describe(event: &api::ResponseEvent) -> String {
    match &event.r#type {
        Some(api::response_event::Type::Init(init)) => {
            format!("Init            run_id={}", init.run_id)
        }
        Some(api::response_event::Type::Finished(finished)) => {
            format!("Finished        {:?}", finished.reason)
        }
        Some(api::response_event::Type::ClientActions(actions)) => actions
            .actions
            .iter()
            .filter_map(|action| action.action.as_ref())
            .map(describe_action)
            .collect::<Vec<_>>()
            .join("\n"),
        None => "empty event".to_string(),
    }
}

fn describe_action(action: &api::client_action::Action) -> String {
    match action {
        api::client_action::Action::BeginTransaction(_) => "  begin".to_string(),
        api::client_action::Action::CommitTransaction(_) => "  commit".to_string(),
        api::client_action::Action::CreateTask(task) => format!(
            "  CreateTask      {}",
            task.task.as_ref().map(|t| t.id.as_str()).unwrap_or("?")
        ),
        api::client_action::Action::AddMessagesToTask(add) => add
            .messages
            .iter()
            .map(|message| format!("  Add             {}", describe_message(message)))
            .collect::<Vec<_>>()
            .join("\n"),
        api::client_action::Action::AppendToMessageContent(append) => format!(
            "  Append          {}",
            append
                .message
                .as_ref()
                .map(describe_message)
                .unwrap_or_default()
        ),
        other => format!("  {other:?}"),
    }
}

fn describe_message(message: &api::Message) -> String {
    match &message.message {
        Some(api::message::Message::AgentOutput(output)) => {
            format!("AgentOutput {:?}", output.text)
        }
        Some(api::message::Message::ToolCall(call)) => format!(
            "ToolCall {} {}",
            call.tool_call_id,
            call.tool
                .as_ref()
                .map(|tool| format!("{tool:?}"))
                .unwrap_or_else(|| "(local only)".to_string())
                .chars()
                .take(160)
                .collect::<String>()
        ),
        Some(api::message::Message::ToolCallResult(result)) => {
            format!("ToolCallResult {}", result.tool_call_id)
        }
        Some(other) => format!("{other:?}").chars().take(120).collect(),
        None => "empty message".to_string(),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let prompt = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "Reply with exactly: PONG".to_string());
    let service = env_or("WARP_LOCAL_AGENT_URL", "http://127.0.0.1:9377");
    let provider_url = env_or("LOCAL_AGENT_PROVIDER_URL", "http://127.0.0.1:11434");
    let api_key = env_or("LOCAL_AGENT_PROVIDER_KEY", "");
    let model = env_or("LOCAL_AGENT_MODEL", "qwen2.5-coder:7b");

    println!("service  {service}");
    println!("provider {provider_url}");
    println!("model    {model}");
    println!("prompt   {prompt:?}\n");

    let request = build_request(&prompt, &provider_url, &api_key, &model);

    println!("--- turn 1 ---");
    let events = post(&service, &request).await?;
    for event in &events {
        println!("{}", describe(event));
    }

    // Answering a deferred call is the whole point of the protocol, so drive the second turn too.
    let Some((task_id, tool_call)) = deferred_tool_call(&events) else {
        return Ok(());
    };
    println!("\n--- turn 2, answering {} ---", tool_call.0);
    let mut follow_up = build_request("", &provider_url, &api_key, &model);
    follow_up.task_context = Some(api::request::TaskContext {
        tasks: vec![api::Task {
            id: task_id.clone(),
            messages: vec![user_query_message(&task_id, &prompt), tool_call.1.clone()],
            ..Default::default()
        }],
    });
    follow_up.input = Some(api::request::Input {
        context: request
            .input
            .as_ref()
            .and_then(|input| input.context.clone()),
        r#type: Some(api::request::input::Type::UserInputs(
            api::request::input::UserInputs {
                inputs: vec![api::request::input::user_inputs::UserInput {
                    input: Some(
                        api::request::input::user_inputs::user_input::Input::ToolCallResult(
                            api::request::input::ToolCallResult {
                                tool_call_id: tool_call.0.clone(),
                                result: Some(shell_result()),
                            },
                        ),
                    ),
                }],
            },
        )),
    });

    for event in post(&service, &follow_up).await? {
        println!("{}", describe(&event));
    }

    Ok(())
}

async fn post(service: &str, request: &api::Request) -> anyhow::Result<Vec<api::ResponseEvent>> {
    let body = reqwest::Client::new()
        .post(format!("{}/ai/multi-agent", service.trim_end_matches('/')))
        .header("content-type", "application/x-protobuf")
        .body(request.encode_to_vec())
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;

    body.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(|line| {
            let bytes = BASE64_URL_SAFE.decode(line.trim().trim_matches('"'))?;
            Ok(api::ResponseEvent::decode(bytes.as_slice())?)
        })
        .collect()
}

/// The task id and the persisted `ToolCall` message the client is expected to execute.
fn deferred_tool_call(events: &[api::ResponseEvent]) -> Option<(String, (String, api::Message))> {
    events
        .iter()
        .filter_map(|event| match &event.r#type {
            Some(api::response_event::Type::ClientActions(actions)) => Some(&actions.actions),
            _ => None,
        })
        .flatten()
        .filter_map(|action| match action.action.as_ref()? {
            api::client_action::Action::AddMessagesToTask(add) => Some(add),
            _ => None,
        })
        .flat_map(|add| add.messages.iter().map(move |m| (add.task_id.clone(), m)))
        .find_map(|(task_id, message)| match &message.message {
            Some(api::message::Message::ToolCall(call)) => {
                Some((task_id, (call.tool_call_id.clone(), message.clone())))
            }
            _ => None,
        })
}

fn user_query_message(task_id: &str, query: &str) -> api::Message {
    api::Message {
        id: uuid::Uuid::new_v4().to_string(),
        task_id: task_id.to_string(),
        message: Some(api::message::Message::UserQuery(api::message::UserQuery {
            query: query.to_string(),
            ..Default::default()
        })),
        ..Default::default()
    }
}

/// A plausible successful shell result, standing in for what the client would have executed.
fn shell_result() -> api::request::input::tool_call_result::Result {
    api::request::input::tool_call_result::Result::RunShellCommand(api::RunShellCommandResult {
        command: "ls -la".to_string(),
        result: Some(api::run_shell_command_result::Result::CommandFinished(
            api::ShellCommandFinished {
                exit_code: 0,
                output: "Cargo.toml\nsrc\nREADME.md\n".to_string(),
                ..Default::default()
            },
        )),
        ..Default::default()
    })
}
