use std::collections::HashMap;
use std::sync::Arc;

use futures_util::StreamExt;
use warp_core::features::FeatureFlag;
use warp_multi_agent_api as api;

use super::convert_to::convert_input;
use super::{ConvertToAPITypeError, OllamaConfig, RequestParams, ResponseStream};
use crate::ai::agent::redaction;
use crate::ai::blocklist::video_recording_enabled;
use crate::server::server_api::{AIApiError, ServerApi};
use crate::server::team_scope::RequestTeamScope;
use crate::terminal::model::session::SessionType;

pub async fn generate_multi_agent_output(
    server_api: Arc<ServerApi>,
    mut params: RequestParams,
    team_scope: RequestTeamScope,
    cancellation_rx: futures::channel::oneshot::Receiver<()>,
) -> Result<ResponseStream, ConvertToAPITypeError> {
    // An Ollama turn is handled by the local agent service, which receives the same request the
    // backend would have, built below.
    let local_agent = params.ollama_config.clone();

    let supported_tools = params
        .supported_tools_override
        .take()
        .unwrap_or_else(|| get_supported_tools(&params));
    let supported_cli_agent_tools = get_supported_cli_agent_tools(&params);
    let mut logging_metadata = HashMap::new();
    if let Some(metadata) = params.metadata {
        logging_metadata.insert(
            "is_autodetected_user_query".to_owned(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::BoolValue(
                    metadata.is_autodetected_user_query,
                )),
            },
        );
        logging_metadata.insert(
            "entrypoint".to_owned(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue(
                    metadata.entrypoint.entrypoint(),
                )),
            },
        );
        logging_metadata.insert(
            "is_auto_resume_after_error".to_owned(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::BoolValue(
                    metadata.is_auto_resume_after_error,
                )),
            },
        );
    }

    if params.should_redact_secrets {
        redaction::redact_inputs(&mut params.input);
    }

    let api_keys = api_keys_with_warp_credit_fallback_setting(
        params.api_keys,
        params.allow_use_of_warp_credits,
    );

    let mut request = api::Request {
        task_context: Some(api::request::TaskContext {
            tasks: params.tasks,
        }),
        input: Some(convert_input(params.input)?),
        settings: Some(api::request::Settings {
            model_config: Some(api::request::settings::ModelConfig {
                base: params.model.into(),
                cli_agent: params.cli_agent_model.into(),
                computer_use_agent: params.computer_use_model.into(),
                base_model_context_window_limit: params.context_window_limit.unwrap_or(0),
                ..Default::default()
            }),
            rules_enabled: params.is_memory_enabled,
            warp_drive_context_enabled: params.warp_drive_context_enabled,
            web_context_retrieval_enabled: true,
            supports_parallel_tool_calls: true,
            use_anthropic_text_editor_tools: false,
            planning_enabled: params.planning_enabled,
            supports_create_files: true,
            supports_create_file_overwrite: true,
            supported_tools: supported_tools.into_iter().map(Into::into).collect(),
            supports_long_running_commands: true,
            should_preserve_file_content_in_history: true,
            supports_todos_ui: true,
            supports_linked_code_blocks: FeatureFlag::LinkedCodeBlocks.is_enabled(),
            supports_started_child_task_message: true,
            supports_suggest_prompt: true,
            supports_read_image_files: FeatureFlag::ReadImageFiles.is_enabled(),
            supports_reasoning_message: true,
            api_keys,
            autonomy_level: params.autonomy_level.into(),
            isolation_level: params.isolation_level.into(),
            web_search_enabled: params.web_search_enabled,
            supported_cli_agent_tools: supported_cli_agent_tools
                .into_iter()
                .map(Into::into)
                .collect(),
            supports_v4a_file_diffs: FeatureFlag::V4AFileDiffs.is_enabled(),
            supports_summarization_via_message_replacement:
                FeatureFlag::SummarizationViaMessageReplacement.is_enabled(),
            supports_bundled_skills: FeatureFlag::BundledSkills.is_enabled(),
            supports_research_agent: params.research_agent_enabled,
            supports_orchestration_v2: supports_orchestration_v2(params.orchestration_enabled),
            supports_orchestration_runners: params.orchestration_enabled
                && FeatureFlag::CloudAgentRunners.is_enabled(),
            supports_background_computer_use: FeatureFlag::BackgroundComputerUse.is_enabled()
                && computer_use::background_supported(),
            supports_stored_screenshots: FeatureFlag::StoredScreenshots.is_enabled(),
            custom_model_providers: params.custom_model_providers,
            custom_model_routers: params.custom_model_routers,
        }),
        metadata: Some(api::request::Metadata {
            logging: logging_metadata,
            conversation_id: params
                .conversation_token
                .as_ref()
                .map(|token| token.as_str().to_string())
                .unwrap_or_default(),
            ambient_agent_task_id: params
                .ambient_agent_task_id
                .map(|id| id.to_string())
                .unwrap_or_default(),
            forked_from_conversation_id: if params.conversation_token.is_none() {
                // We only include this param on our initial request to the server
                // (when the forked conversation has not been assigned a new id yet).
                params
                    .forked_from_conversation_token
                    .map(|token| token.as_str().to_string())
                    .unwrap_or_default()
            } else {
                String::new()
            },
            parent_agent_id: params.parent_agent_id.unwrap_or_default(),
            agent_name: params.agent_name.unwrap_or_default(),
        }),
        existing_suggestions: params
            .existing_suggestions
            .map(|suggestions| suggestions.into()),
        mcp_context: params.mcp_context.map(Into::into),
    };

    let local_agent_url = local_agent.as_ref().map(|cfg| cfg.service_url.clone());
    let response_stream = match &local_agent {
        Some(cfg) => {
            apply_local_agent_settings(&mut request, cfg);
            warp_multi_agent_client::generate_local_agent_output(
                server_api.as_ref().http_client(),
                &cfg.service_url,
                &request,
            )
            .await
        }
        None => {
            warp_multi_agent_client::generate_multi_agent_output(
                server_api.as_ref(),
                &request,
                team_scope.team_uid().map(|uid| uid.uid()),
            )
            .await
        }
    };
    match response_stream {
        Ok(stream) => {
            let output_stream = stream
                .then(move |result| {
                    let local_agent_url = local_agent_url.clone();
                    async move {
                        match result {
                            Ok(event) => Ok(event),
                            Err(error) => Err(convert_multi_agent_client_error(
                                error,
                                local_agent_url.as_deref(),
                            )
                            .await),
                        }
                    }
                })
                .take_until(cancellation_rx);
            Ok(Box::pin(output_stream))
        }
        Err(e) => {
            let (tx, rx) = async_channel::unbounded();
            let _ = tx
                .send(Err(convert_multi_agent_client_error(
                    e,
                    local_agent_url.as_deref(),
                )
                .await))
                .await;
            Ok(Box::pin(rx))
        }
    }
}

/// The config key that ties `model_config.base` to the provider entry the service reads the
/// Ollama endpoint from.
const LOCAL_OLLAMA_CONFIG_KEY: &str = "local-ollama";

/// Point a request at the local agent service.
///
/// The service has no Warp credentials and picks its provider out of `custom_model_providers`, so
/// the user's Ollama endpoint travels with the request and Warp's own keys are dropped.
fn apply_local_agent_settings(request: &mut api::Request, config: &OllamaConfig) {
    use api::request::settings::custom_model_providers::{
        CustomEndpointSchema, CustomModel, CustomModelProvider,
    };

    let Some(settings) = request.settings.as_mut() else {
        return;
    };
    if let Some(model_config) = settings.model_config.as_mut() {
        model_config.base = LOCAL_OLLAMA_CONFIG_KEY.to_string();
    }
    settings.api_keys = None;
    settings.custom_model_providers = Some(api::request::settings::CustomModelProviders {
        providers: vec![CustomModelProvider {
            base_url: config.base_url.clone(),
            api_key: config.api_key.clone().unwrap_or_default(),
            schema: CustomEndpointSchema::OpenaiChatCompletions as i32,
            models: vec![CustomModel {
                slug: config.model.clone(),
                config_key: LOCAL_OLLAMA_CONFIG_KEY.to_string(),
                reasoning_effort: String::new(),
            }],
        }],
    });
}

async fn convert_multi_agent_client_error(
    error: warp_multi_agent_client::Error,
    local_agent_url: Option<&str>,
) -> Arc<AIApiError> {
    let error = match error {
        warp_multi_agent_client::Error::Authentication(error)
        | warp_multi_agent_client::Error::AmbientHeaders(error) => AIApiError::Other(error),
        warp_multi_agent_client::Error::Base64Decode(error) => {
            AIApiError::Other(anyhow::Error::from(error))
        }
        warp_multi_agent_client::Error::ProtobufDecode(error) => {
            AIApiError::Other(anyhow::Error::from(error))
        }
        // A local service that is simply not running is the common failure here, and the
        // transport error alone does not say how to fix it.
        warp_multi_agent_client::Error::EventSource(error) => match local_agent_url {
            Some(url) => AIApiError::Other(anyhow::anyhow!(
                "Could not reach the local agent service at {url}. Start it with \
                 `cargo run -p warp_local_agent`. ({error:?})"
            )),
            None => AIApiError::from_stream_error("GenerateMultiAgentOutput", *error).await,
        },
    };
    Arc::new(error)
}

fn api_keys_with_warp_credit_fallback_setting(
    api_keys: Option<api::request::settings::ApiKeys>,
    allow_use_of_warp_credits: bool,
) -> Option<api::request::settings::ApiKeys> {
    match api_keys {
        Some(mut api_keys) => {
            api_keys.allow_use_of_warp_credits = allow_use_of_warp_credits;
            Some(api_keys)
        }
        None if allow_use_of_warp_credits => Some(api::request::settings::ApiKeys {
            allow_use_of_warp_credits: true,
            ..Default::default()
        }),
        None => None,
    }
}

fn supports_orchestration_v2(orchestration_enabled: bool) -> bool {
    orchestration_enabled
}

fn get_supported_tools(params: &RequestParams) -> Vec<api::ToolType> {
    let mut supported_tools = vec![
        api::ToolType::Grep,
        api::ToolType::FileGlob,
        api::ToolType::FileGlobV2,
        api::ToolType::ReadMcpResource,
        api::ToolType::CallMcpTool,
        api::ToolType::InitProject,
        api::ToolType::OpenCodeReview,
        api::ToolType::RunShellCommand,
        api::ToolType::SuggestNewConversation,
        api::ToolType::Subagent,
        api::ToolType::WriteToLongRunningShellCommand,
        api::ToolType::ReadShellCommandOutput,
        api::ToolType::ReadDocuments,
        api::ToolType::CreateDocuments,
        api::ToolType::EditDocuments,
        api::ToolType::SuggestPrompt,
    ];

    if FeatureFlag::ConversationsAsContext.is_enabled() {
        supported_tools.push(api::ToolType::FetchConversation);
    }

    match params.session_context.session_type() {
        None | Some(SessionType::Local) => {
            supported_tools.extend(&[
                api::ToolType::ReadFiles,
                api::ToolType::ApplyFileDiffs,
                api::ToolType::SearchCodebase,
            ]);

            if FeatureFlag::ArtifactCommand.is_enabled() {
                supported_tools.push(api::ToolType::UploadFileArtifact);
            }
        }
        Some(SessionType::WarpifiedRemote { host_id: Some(_) }) => {
            // Remote session with a known host — enable tools that route
            // through RemoteServerClient. The host_id is only populated
            // after a successful connection handshake, so its presence is a
            // sufficient proxy for client availability.
            supported_tools.extend(&[api::ToolType::ReadFiles, api::ToolType::ApplyFileDiffs]);
            if FeatureFlag::RemoteCodebaseIndexing.is_enabled() {
                supported_tools.push(api::ToolType::SearchCodebase);
            }
        }
        Some(SessionType::WarpifiedRemote { host_id: None }) => {
            // Feature flag off or not yet connected — no remote tools.
        }
    }

    if FeatureFlag::AgentModeComputerUse.is_enabled() && params.computer_use_enabled {
        supported_tools.extend(&[api::ToolType::UseComputer]);
        supported_tools.extend(&[api::ToolType::RequestComputerUse]);

        if video_recording_enabled() {
            supported_tools.extend(&[api::ToolType::StartRecording, api::ToolType::StopRecording]);
        }
    }

    supported_tools.push(api::ToolType::InsertReviewComments);

    if FeatureFlag::ListSkills.is_enabled() {
        supported_tools.push(api::ToolType::ReadSkill);
    }

    if params.orchestration_enabled {
        supported_tools.extend([api::ToolType::RunAgents, api::ToolType::SendMessageToAgent]);
        // Declare client-handled wait_for_events so the server doesn't
        // fall back to the legacy server-handled form.
        supported_tools.push(api::ToolType::WaitForEvents);
    }

    if FeatureFlag::AskUserQuestion.is_enabled() && params.ask_user_question_enabled {
        supported_tools.push(api::ToolType::AskUserQuestion);
    }

    supported_tools
}

fn get_supported_cli_agent_tools(params: &RequestParams) -> Vec<api::ToolType> {
    let mut supported_cli_agent_tools = vec![
        api::ToolType::WriteToLongRunningShellCommand,
        api::ToolType::ReadShellCommandOutput,
        api::ToolType::Grep,
        api::ToolType::FileGlob,
        api::ToolType::FileGlobV2,
    ];

    if FeatureFlag::TransferControlTool.is_enabled() {
        supported_cli_agent_tools.push(api::ToolType::TransferShellCommandControlToUser);
    }

    match params.session_context.session_type() {
        None | Some(SessionType::Local) => {
            supported_cli_agent_tools
                .extend(&[api::ToolType::ReadFiles, api::ToolType::SearchCodebase]);
        }
        Some(SessionType::WarpifiedRemote { host_id: Some(_) }) => {
            supported_cli_agent_tools.push(api::ToolType::ReadFiles);
            if FeatureFlag::RemoteCodebaseIndexing.is_enabled() {
                supported_cli_agent_tools.push(api::ToolType::SearchCodebase);
            }
        }
        Some(SessionType::WarpifiedRemote { host_id: None }) => {}
    }

    supported_cli_agent_tools
}

#[cfg(test)]
#[path = "impl_tests.rs"]
mod tests;
