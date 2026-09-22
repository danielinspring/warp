//! Renders client-executed tool results for the local model and echoes them back to the client as
//! persisted `ToolCallResult` messages.

use local_agent_runtime::{ToolCallResult, encode_local_runtime_tool_result_data};
use serde_json::{Value, json};
use uuid::Uuid;
use warp_multi_agent_api as api;
use warp_multi_agent_api::ask_user_question_result::answer_item;
use warp_multi_agent_api::call_mcp_tool_result::success::Result as McpToolResultItem;
use warp_multi_agent_api::call_mcp_tool_result::success::result::Result as McpToolContent;
use warp_multi_agent_api::message::tool_call_result::Result as MessageResult;
use warp_multi_agent_api::request::input::tool_call_result::Result as RequestResult;
use warp_multi_agent_api::run_agents::execution_mode::Mode as ExecutionMode;
use warp_multi_agent_api::run_agents_result::agent_outcome;
use warp_multi_agent_api::{
    any_file_content, apply_file_diffs_result, ask_user_question_result, call_mcp_tool_result,
    create_documents_result, edit_documents_result, fetch_conversation_result, file_glob_result,
    file_glob_v2_result, grep_result, insert_review_comments_result, mcp_resource_content,
    permission_denied, read_documents_result, read_files_result, read_mcp_resource_result,
    read_shell_command_output_result, read_skill_result, request_computer_use_result,
    run_agents_result, run_shell_command_result, search_codebase_result,
    send_message_to_agent_result, shell_command_error, start_recording_result,
    stop_recording_result, suggest_new_conversation_result, suggest_plan_result,
    suggest_prompt_result, transfer_shell_command_control_to_user_result,
    upload_file_artifact_result, use_computer_result, write_to_long_running_shell_command_result,
};

/// Renders a client-executed tool result as model-facing text plus an error flag.
///
/// A result without a payload means the client cancelled the call.
pub fn render_tool_call_result(result: &api::request::input::ToolCallResult) -> ToolCallResult {
    match &result.result {
        Some(result) => ToolCallResult {
            content: render_content(result),
            is_error: !is_successful(result),
        },
        None => ToolCallResult::error("Tool call cancelled"),
    }
}

/// Re-wraps a request-side result payload into the message-side oneof used for persistence.
pub fn to_message_result(result: RequestResult) -> MessageResult {
    match result {
        RequestResult::RunShellCommand(result) => MessageResult::RunShellCommand(result),
        RequestResult::ReadFiles(result) => MessageResult::ReadFiles(result),
        RequestResult::SearchCodebase(result) => MessageResult::SearchCodebase(result),
        RequestResult::ApplyFileDiffs(result) => MessageResult::ApplyFileDiffs(result),
        RequestResult::SuggestPlan(result) => MessageResult::SuggestPlan(result),
        RequestResult::SuggestCreatePlan(result) => MessageResult::SuggestCreatePlan(result),
        RequestResult::Grep(result) => MessageResult::Grep(result),
        #[allow(deprecated)]
        RequestResult::FileGlob(result) => MessageResult::FileGlob(result),
        RequestResult::ReadMcpResource(result) => MessageResult::ReadMcpResource(result),
        RequestResult::CallMcpTool(result) => MessageResult::CallMcpTool(result),
        RequestResult::WriteToLongRunningShellCommand(result) => {
            MessageResult::WriteToLongRunningShellCommand(result)
        }
        RequestResult::SuggestNewConversation(result) => {
            MessageResult::SuggestNewConversation(result)
        }
        RequestResult::FileGlobV2(result) => MessageResult::FileGlobV2(result),
        RequestResult::SuggestPrompt(result) => MessageResult::SuggestPrompt(result),
        RequestResult::OpenCodeReview(result) => MessageResult::OpenCodeReview(result),
        RequestResult::InitProject(result) => MessageResult::InitProject(result),
        RequestResult::ReadDocuments(result) => MessageResult::ReadDocuments(result),
        RequestResult::EditDocuments(result) => MessageResult::EditDocuments(result),
        RequestResult::CreateDocuments(result) => MessageResult::CreateDocuments(result),
        RequestResult::ReadShellCommandOutput(result) => {
            MessageResult::ReadShellCommandOutput(result)
        }
        RequestResult::UseComputer(result) => MessageResult::UseComputer(result),
        RequestResult::InsertReviewComments(result) => MessageResult::InsertReviewComments(result),
        RequestResult::RequestComputerUse(result) => {
            MessageResult::RequestComputerUseResult(result)
        }
        RequestResult::ReadSkill(result) => MessageResult::ReadSkill(result),
        RequestResult::FetchConversation(result) => MessageResult::FetchConversation(result),
        RequestResult::SendMessageToAgent(result) => MessageResult::SendMessageToAgent(result),
        RequestResult::TransferShellCommandControlToUser(result) => {
            MessageResult::TransferShellCommandControlToUser(result)
        }
        RequestResult::AskUserQuestion(result) => MessageResult::AskUserQuestion(result),
        RequestResult::UploadFileArtifact(result) => MessageResult::UploadFileArtifact(result),
        RequestResult::RunAgentsResult(result) => MessageResult::RunAgentsResult(result),
        RequestResult::WaitForEvents(result) => MessageResult::WaitForEvents(result),
        RequestResult::StartRecording(result) => MessageResult::StartRecording(result),
        RequestResult::StopRecording(result) => MessageResult::StopRecording(result),
    }
}

/// Builds the `ToolCallResult` message echoed to the client so the result is persisted in the
/// conversation. `rendered` is the model-facing rendering of `result`, stored as server message
/// data so the transcript can be replayed to the local model later.
pub fn tool_call_result_message(
    task_id: &str,
    request_id: &str,
    result: &api::request::input::ToolCallResult,
    rendered: &ToolCallResult,
) -> api::Message {
    let proto_result = match result.result.clone() {
        Some(result) => to_message_result(result),
        None => MessageResult::Cancel(()),
    };

    api::Message {
        id: Uuid::new_v4().to_string(),
        task_id: task_id.to_string(),
        request_id: request_id.to_string(),
        timestamp: None,
        server_message_data: encode_local_runtime_tool_result_data(&result.tool_call_id, rendered),
        citations: vec![],
        fetched_memories: vec![],
        message: Some(api::message::Message::ToolCallResult(
            api::message::ToolCallResult {
                tool_call_id: result.tool_call_id.clone(),
                context: None,
                result: Some(proto_result),
            },
        )),
    }
}

fn is_successful(result: &RequestResult) -> bool {
    match result {
        RequestResult::RunShellCommand(result) => match &result.result {
            Some(run_shell_command_result::Result::CommandFinished(finished)) => {
                finished.exit_code == 0
            }
            Some(run_shell_command_result::Result::LongRunningCommandSnapshot(_)) => true,
            Some(run_shell_command_result::Result::PermissionDenied(_)) | None => false,
        },
        RequestResult::ReadFiles(result) => matches!(
            result.result,
            Some(
                read_files_result::Result::TextFilesSuccess(_)
                    | read_files_result::Result::AnyFilesSuccess(_)
            )
        ),
        RequestResult::SearchCodebase(result) => {
            matches!(
                result.result,
                Some(search_codebase_result::Result::Success(_))
            )
        }
        RequestResult::ApplyFileDiffs(result) => {
            matches!(
                result.result,
                Some(apply_file_diffs_result::Result::Success(_))
            )
        }
        RequestResult::SuggestPlan(result) => result.result.is_some(),
        RequestResult::SuggestCreatePlan(result) => result.accepted,
        RequestResult::Grep(result) => {
            matches!(result.result, Some(grep_result::Result::Success(_)))
        }
        RequestResult::FileGlob(result) => {
            matches!(result.result, Some(file_glob_result::Result::Success(_)))
        }
        RequestResult::ReadMcpResource(result) => {
            matches!(
                result.result,
                Some(read_mcp_resource_result::Result::Success(_))
            )
        }
        RequestResult::CallMcpTool(result) => {
            matches!(
                result.result,
                Some(call_mcp_tool_result::Result::Success(_))
            )
        }
        RequestResult::WriteToLongRunningShellCommand(result) => matches!(
            result.result,
            Some(
                write_to_long_running_shell_command_result::Result::LongRunningCommandSnapshot(_)
                    | write_to_long_running_shell_command_result::Result::CommandFinished(_)
            )
        ),
        RequestResult::SuggestNewConversation(result) => matches!(
            result.result,
            Some(suggest_new_conversation_result::Result::Accepted(_))
        ),
        RequestResult::FileGlobV2(result) => {
            matches!(result.result, Some(file_glob_v2_result::Result::Success(_)))
        }
        RequestResult::SuggestPrompt(result) => {
            matches!(
                result.result,
                Some(suggest_prompt_result::Result::Accepted(()))
            )
        }
        RequestResult::OpenCodeReview(_)
        | RequestResult::InitProject(_)
        | RequestResult::WaitForEvents(_) => true,
        RequestResult::ReadDocuments(result) => {
            matches!(
                result.result,
                Some(read_documents_result::Result::Success(_))
            )
        }
        RequestResult::EditDocuments(result) => {
            matches!(
                result.result,
                Some(edit_documents_result::Result::Success(_))
            )
        }
        RequestResult::CreateDocuments(result) => {
            matches!(
                result.result,
                Some(create_documents_result::Result::Success(_))
            )
        }
        RequestResult::ReadShellCommandOutput(result) => matches!(
            result.result,
            Some(
                read_shell_command_output_result::Result::LongRunningCommandSnapshot(_)
                    | read_shell_command_output_result::Result::CommandFinished(_)
            )
        ),
        RequestResult::UseComputer(result) => {
            matches!(result.result, Some(use_computer_result::Result::Success(_)))
        }
        RequestResult::InsertReviewComments(result) => matches!(
            result.result,
            Some(insert_review_comments_result::Result::Success(_))
        ),
        RequestResult::RequestComputerUse(result) => matches!(
            result.result,
            Some(request_computer_use_result::Result::Approved(_))
        ),
        RequestResult::ReadSkill(result) => {
            matches!(result.result, Some(read_skill_result::Result::Success(_)))
        }
        RequestResult::FetchConversation(result) => {
            matches!(
                result.result,
                Some(fetch_conversation_result::Result::Success(_))
            )
        }
        RequestResult::SendMessageToAgent(result) => matches!(
            result.result,
            Some(send_message_to_agent_result::Result::Success(_))
        ),
        RequestResult::TransferShellCommandControlToUser(result) => matches!(
            result.result,
            Some(
                transfer_shell_command_control_to_user_result::Result::LongRunningCommandSnapshot(
                    _
                ) | transfer_shell_command_control_to_user_result::Result::CommandFinished(_)
            )
        ),
        RequestResult::AskUserQuestion(result) => {
            matches!(
                result.result,
                Some(ask_user_question_result::Result::Success(_))
            )
        }
        RequestResult::UploadFileArtifact(result) => matches!(
            result.result,
            Some(upload_file_artifact_result::Result::Success(_))
        ),
        RequestResult::RunAgentsResult(result) => match &result.outcome {
            Some(run_agents_result::Outcome::Launched(launched)) => launched
                .agents
                .iter()
                .any(|agent| matches!(agent.result, Some(agent_outcome::Result::Launched(_)))),
            Some(
                run_agents_result::Outcome::Denied(_) | run_agents_result::Outcome::Failure(_),
            )
            | None => false,
        },
        RequestResult::StartRecording(result) => {
            matches!(
                result.result,
                Some(start_recording_result::Result::Success(_))
            )
        }
        RequestResult::StopRecording(result) => matches!(
            result.result,
            Some(
                stop_recording_result::Result::Success(_)
                    | stop_recording_result::Result::Discarded(_)
            )
        ),
    }
}

fn render_content(result: &RequestResult) -> String {
    match result {
        RequestResult::RunShellCommand(result) => render_run_shell_command(result),
        RequestResult::ReadFiles(result) => render_read_files(result),
        RequestResult::SearchCodebase(result) => match &result.result {
            Some(search_codebase_result::Result::Success(success)) => {
                file_contents_to_json(success.files.iter().map(file_content_json))
            }
            Some(search_codebase_result::Result::Error(error)) => error_json(&error.message),
            None => missing_result_json("search_codebase"),
        },
        RequestResult::ApplyFileDiffs(result) => render_apply_file_diffs(result),
        RequestResult::SuggestPlan(result) => match &result.result {
            Some(suggest_plan_result::Result::Accepted(())) => {
                json!({ "status": "accepted" }).to_string()
            }
            Some(suggest_plan_result::Result::UserEditedPlan(edited)) => json!({
                "status": "accepted",
                "user_edited": true,
                "plan_text": edited.plan_text,
            })
            .to_string(),
            None => missing_result_json("suggest_plan"),
        },
        RequestResult::SuggestCreatePlan(result) => {
            let status = if result.accepted {
                "accepted"
            } else {
                "rejected"
            };
            json!({ "status": status }).to_string()
        }
        RequestResult::Grep(result) => match &result.result {
            Some(grep_result::Result::Success(success)) => {
                let matched_files = success
                    .matched_files
                    .iter()
                    .map(|file| {
                        let matched_lines = file
                            .matched_lines
                            .iter()
                            .map(|line| json!({ "line_number": line.line_number }))
                            .collect::<Vec<_>>();
                        json!({
                            "file_path": file.file_path,
                            "matched_lines": matched_lines,
                        })
                    })
                    .collect::<Vec<_>>();
                json!({ "matched_files": matched_files }).to_string()
            }
            Some(grep_result::Result::Error(error)) => error_json(&error.message),
            None => missing_result_json("grep"),
        },
        RequestResult::FileGlob(result) => match &result.result {
            Some(file_glob_result::Result::Success(success)) => {
                json!({ "matched_files": success.matched_files }).to_string()
            }
            Some(file_glob_result::Result::Error(error)) => error_json(&error.message),
            None => missing_result_json("file_glob"),
        },
        RequestResult::ReadMcpResource(result) => match &result.result {
            Some(read_mcp_resource_result::Result::Success(success)) => {
                let resource_contents = success
                    .contents
                    .iter()
                    .map(mcp_resource_content_json)
                    .collect::<Vec<_>>();
                json!({ "resource_contents": resource_contents }).to_string()
            }
            Some(read_mcp_resource_result::Result::Error(error)) => bare_error_json(&error.message),
            None => missing_result_json("read_mcp_resource"),
        },
        RequestResult::CallMcpTool(result) => match &result.result {
            Some(call_mcp_tool_result::Result::Success(success)) => {
                let content = success
                    .results
                    .iter()
                    .filter_map(mcp_tool_content_json)
                    .collect::<Vec<_>>();
                json!({ "result": { "content": content } }).to_string()
            }
            Some(call_mcp_tool_result::Result::Error(error)) => bare_error_json(&error.message),
            None => missing_result_json("call_mcp_tool"),
        },
        RequestResult::WriteToLongRunningShellCommand(result) => {
            render_write_to_long_running_shell_command(result)
        }
        RequestResult::SuggestNewConversation(result) => match &result.result {
            Some(suggest_new_conversation_result::Result::Accepted(accepted)) => json!({
                "status": "accepted",
                "message_id": accepted.message_id,
            })
            .to_string(),
            Some(suggest_new_conversation_result::Result::Rejected(_)) => {
                json!({ "status": "rejected" }).to_string()
            }
            None => missing_result_json("suggest_new_conversation"),
        },
        RequestResult::FileGlobV2(result) => match &result.result {
            Some(file_glob_v2_result::Result::Success(success)) => {
                let matched_files = success
                    .matched_files
                    .iter()
                    .map(|file| json!({ "file_path": file.file_path }))
                    .collect::<Vec<_>>();
                json!({
                    "matched_files": matched_files,
                    "warnings": (!success.warnings.is_empty()).then_some(&success.warnings),
                })
                .to_string()
            }
            Some(file_glob_v2_result::Result::Error(error)) => error_json(&error.message),
            None => missing_result_json("file_glob"),
        },
        RequestResult::SuggestPrompt(result) => match &result.result {
            Some(suggest_prompt_result::Result::Accepted(())) => {
                json!({ "status": "accepted" }).to_string()
            }
            Some(suggest_prompt_result::Result::Rejected(())) => {
                json!({ "status": "rejected" }).to_string()
            }
            None => missing_result_json("suggest_prompt"),
        },
        RequestResult::OpenCodeReview(_)
        | RequestResult::InitProject(_)
        | RequestResult::WaitForEvents(_) => json!({ "status": "completed" }).to_string(),
        RequestResult::ReadDocuments(result) => match &result.result {
            Some(read_documents_result::Result::Success(success)) => {
                document_contents_to_json("completed", &success.documents)
            }
            Some(read_documents_result::Result::Error(error)) => error_json(&error.message),
            None => missing_result_json("read_documents"),
        },
        RequestResult::EditDocuments(result) => match &result.result {
            Some(edit_documents_result::Result::Success(success)) => {
                document_contents_to_json("accepted", &success.updated_documents)
            }
            Some(edit_documents_result::Result::Error(error)) => error_json(&error.message),
            None => missing_result_json("edit_documents"),
        },
        RequestResult::CreateDocuments(result) => match &result.result {
            Some(create_documents_result::Result::Success(success)) => {
                document_contents_to_json("created", &success.created_documents)
            }
            Some(create_documents_result::Result::Error(error)) => error_json(&error.message),
            None => missing_result_json("create_documents"),
        },
        RequestResult::ReadShellCommandOutput(result) => render_read_shell_command_output(result),
        RequestResult::UseComputer(result) => match &result.result {
            Some(use_computer_result::Result::Success(success)) => {
                let cursor_position = success
                    .cursor_position
                    .as_ref()
                    .map(|position| json!({ "x": position.x, "y": position.y }));
                json!({
                    "status": "completed",
                    "screenshot": image_dimensions_json(success.screenshot.as_ref()),
                    "cursor_position": cursor_position,
                    "instruction": "Computer actions finished. Screenshot pixels are not embedded for local models.",
                })
                .to_string()
            }
            Some(use_computer_result::Result::Error(error)) => error_json(&error.message),
            None => missing_result_json("use_computer"),
        },
        RequestResult::InsertReviewComments(result) => match &result.result {
            Some(insert_review_comments_result::Result::Success(_)) => json!({
                "status": "completed",
                "repo_path": result.repo_path,
            })
            .to_string(),
            Some(insert_review_comments_result::Result::Error(error)) => json!({
                "status": "error",
                "repo_path": result.repo_path,
                "error": error.message,
            })
            .to_string(),
            None => missing_result_json("insert_review_comments"),
        },
        RequestResult::RequestComputerUse(result) => render_request_computer_use(result),
        RequestResult::ReadSkill(result) => match &result.result {
            Some(read_skill_result::Result::Success(success)) => {
                file_contents_to_json(success.content.iter().map(file_content_json))
            }
            Some(read_skill_result::Result::Error(error)) => bare_error_json(&error.message),
            None => missing_result_json("read_skill"),
        },
        RequestResult::FetchConversation(result) => match &result.result {
            Some(fetch_conversation_result::Result::Success(success)) => json!({
                "status": "completed",
                "directory_path": success.directory_path,
            })
            .to_string(),
            Some(fetch_conversation_result::Result::Error(error)) => error_json(&error.message),
            None => missing_result_json("fetch_conversation"),
        },
        RequestResult::SendMessageToAgent(result) => match &result.result {
            Some(send_message_to_agent_result::Result::Success(success)) => json!({
                "status": "sent",
                "message_id": success.message_id,
            })
            .to_string(),
            Some(send_message_to_agent_result::Result::Error(error)) => error_json(&error.message),
            None => missing_result_json("send_message_to_agent"),
        },
        RequestResult::TransferShellCommandControlToUser(result) => {
            render_transfer_shell_command_control_to_user(result)
        }
        RequestResult::AskUserQuestion(result) => render_ask_user_question(result),
        RequestResult::UploadFileArtifact(result) => match &result.result {
            Some(upload_file_artifact_result::Result::Success(success)) => json!({
                "status": "uploaded",
                "artifact_uid": success.artifact_uid,
                "mime_type": success.mime_type,
                "size_bytes": success.size_bytes,
            })
            .to_string(),
            Some(upload_file_artifact_result::Result::Error(error)) => error_json(&error.message),
            None => missing_result_json("upload_file_artifact"),
        },
        RequestResult::RunAgentsResult(result) => render_run_agents(result),
        RequestResult::StartRecording(result) => match &result.result {
            Some(start_recording_result::Result::Success(success)) => {
                let capture = success.settings.as_ref().map(
                    |settings| json!({ "width": settings.width_px, "height": settings.height_px }),
                );
                json!({
                    "status": "recording",
                    "recording_id": success.recording_id,
                    "capture": capture,
                })
                .to_string()
            }
            Some(start_recording_result::Result::Error(error)) => error_json(&error.message),
            None => missing_result_json("start_recording"),
        },
        RequestResult::StopRecording(result) => match &result.result {
            Some(stop_recording_result::Result::Success(success)) => json!({
                "status": "stopped",
                "artifact_uid": success.artifact_uid,
                "duration_seconds": success.duration.as_ref().map(|duration| duration.seconds),
                "width": success.width_px,
                "height": success.height_px,
                "size_bytes": success.size_bytes,
                "completion_status": enum_name::<stop_recording_result::CompletionStatus>(
                    success.completion_status,
                ),
                "termination_reason": success.termination_reason,
            })
            .to_string(),
            Some(stop_recording_result::Result::Discarded(_)) => {
                json!({ "status": "discarded" }).to_string()
            }
            Some(stop_recording_result::Result::Error(error)) => error_json(&error.message),
            None => missing_result_json("stop_recording"),
        },
    }
}

fn render_run_shell_command(result: &api::RunShellCommandResult) -> String {
    match &result.result {
        Some(run_shell_command_result::Result::CommandFinished(finished)) => {
            let successful = finished.exit_code == 0;
            json!({
                "status": shell_status(finished.exit_code),
                "exit_code": finished.exit_code,
                "stdout": finished.output,
                "command": result.command,
                "instruction": if successful {
                    "Command finished successfully. Answer the user using stdout. Do not invent timeouts or claim failure."
                } else {
                    "Command failed. Report exit_code and stdout/stderr to the user."
                },
            })
            .to_string()
        }
        Some(run_shell_command_result::Result::LongRunningCommandSnapshot(snapshot)) => json!({
            "status": "long_running",
            "stdout": snapshot.output,
            "command": result.command,
            "block_id": snapshot.command_id,
            "instruction": "Command is still running. If stdout already answers the user, report that answer. Otherwise poll with read_shell_command_output or send input with write_to_long_running_shell_command using this block_id. Do not invent timeouts.",
        })
        .to_string(),
        Some(run_shell_command_result::Result::PermissionDenied(denied)) => {
            let error = match denied.reason {
                Some(permission_denied::Reason::DenylistedCommand(())) => {
                    "Command is on the denylist and cannot be executed."
                }
                None => "Command was denied and cannot be executed.",
            };
            json!({
                "status": "denied",
                "command": result.command,
                "error": error,
            })
            .to_string()
        }
        // A shell result with no typed outcome means the command never ran.
        None => json!({
            "status": "cancelled",
            "error": "Shell command was cancelled before execution. A previous command may still be running in the terminal, or the user dismissed approval. Wait for the active command to finish, then retry with a single scoped read-only command.",
        })
        .to_string(),
    }
}

fn render_read_files(result: &api::ReadFilesResult) -> String {
    match &result.result {
        Some(read_files_result::Result::TextFilesSuccess(success)) => files_json(
            success.files.iter().map(file_content_json).collect(),
            &success.failed_reads,
        ),
        Some(read_files_result::Result::AnyFilesSuccess(success)) => files_json(
            success
                .files
                .iter()
                .filter_map(any_file_content_json)
                .collect(),
            &success.failed_reads,
        ),
        Some(read_files_result::Result::Error(error)) => error_json(&error.message),
        None => missing_result_json("read_files"),
    }
}

fn render_apply_file_diffs(result: &api::ApplyFileDiffsResult) -> String {
    match &result.result {
        Some(apply_file_diffs_result::Result::Success(success)) => {
            let updated_files = success
                .updated_files_v2
                .iter()
                .map(|updated| {
                    let file = updated.file.as_ref().map(|file| {
                        json!({
                            "path": file.file_path,
                            "line_count": file.content.lines().count(),
                        })
                    });
                    json!({
                        "was_edited_by_user": updated.was_edited_by_user,
                        "file": file,
                    })
                })
                .collect::<Vec<_>>();
            let deleted_files = success
                .deleted_files
                .iter()
                .map(|deleted| &deleted.file_path)
                .collect::<Vec<_>>();
            json!({
                "status": "accepted",
                "updated_files": updated_files,
                "deleted_files": deleted_files,
            })
            .to_string()
        }
        Some(apply_file_diffs_result::Result::Error(error)) => error_json(&error.message),
        None => missing_result_json("edit_files"),
    }
}

fn render_write_to_long_running_shell_command(
    result: &api::WriteToLongRunningShellCommandResult,
) -> String {
    match &result.result {
        Some(write_to_long_running_shell_command_result::Result::LongRunningCommandSnapshot(
            snapshot,
        )) => json!({
            "status": "long_running",
            "block_id": snapshot.command_id,
            "stdout": snapshot.output,
            "instruction": "Input was written; command still running. Poll with read_shell_command_output if needed.",
        })
        .to_string(),
        Some(write_to_long_running_shell_command_result::Result::CommandFinished(finished)) => {
            json!({
                "status": shell_status(finished.exit_code),
                "block_id": finished.command_id,
                "exit_code": finished.exit_code,
                "stdout": finished.output,
                "instruction": "Command finished after write. Answer from stdout.",
            })
            .to_string()
        }
        Some(write_to_long_running_shell_command_result::Result::Error(error)) => {
            error_json(&shell_command_error_message(error))
        }
        None => missing_result_json("write_to_long_running_shell_command"),
    }
}

fn render_read_shell_command_output(result: &api::ReadShellCommandOutputResult) -> String {
    match &result.result {
        Some(read_shell_command_output_result::Result::CommandFinished(finished)) => json!({
            "status": shell_status(finished.exit_code),
            "block_id": finished.command_id,
            "exit_code": finished.exit_code,
            "stdout": finished.output,
            "command": result.command,
            "instruction": "Long-running command finished. Answer from stdout; do not invent timeouts.",
        })
        .to_string(),
        Some(read_shell_command_output_result::Result::LongRunningCommandSnapshot(snapshot)) => {
            json!({
                "status": "long_running",
                "block_id": snapshot.command_id,
                "stdout": snapshot.output,
                "command": result.command,
                "instruction": "Still running. Poll again with read_shell_command_output or write with write_to_long_running_shell_command using block_id.",
            })
            .to_string()
        }
        Some(read_shell_command_output_result::Result::Error(error)) => {
            error_json(&shell_command_error_message(error))
        }
        None => missing_result_json("read_shell_command_output"),
    }
}

fn render_transfer_shell_command_control_to_user(
    result: &api::TransferShellCommandControlToUserResult,
) -> String {
    match &result.result {
        Some(
            transfer_shell_command_control_to_user_result::Result::LongRunningCommandSnapshot(
                snapshot,
            ),
        ) => json!({
            "status": "long_running",
            "block_id": snapshot.command_id,
            "stdout": snapshot.output,
            "instruction": "Control was handed to the user; command still running. Poll with read_shell_command_output if needed.",
        })
        .to_string(),
        Some(transfer_shell_command_control_to_user_result::Result::CommandFinished(finished)) => {
            json!({
                "status": shell_status(finished.exit_code),
                "block_id": finished.command_id,
                "exit_code": finished.exit_code,
                "stdout": finished.output,
                "instruction": "Command finished while the user had control. Answer from stdout.",
            })
            .to_string()
        }
        Some(transfer_shell_command_control_to_user_result::Result::Error(error)) => {
            error_json(&shell_command_error_message(error))
        }
        None => missing_result_json("transfer_shell_command_control_to_user"),
    }
}

fn render_request_computer_use(result: &api::RequestComputerUseResult) -> String {
    match &result.result {
        Some(request_computer_use_result::Result::Approved(approved)) => {
            let screenshot = match &approved.screen_dimensions {
                Some(dimensions) => json!({
                    "width": dimensions.width_px,
                    "height": dimensions.height_px,
                }),
                None => image_dimensions_json(approved.initial_screenshot.as_ref()),
            };
            json!({
                "status": "approved",
                "platform": enum_name::<request_computer_use_result::approved::Platform>(
                    approved.platform,
                ),
                "screenshot": screenshot,
                "instruction": "Computer use approved. Proceed with use_computer actions. Screenshot image bytes are not embedded; use dimensions for coordinate planning.",
            })
            .to_string()
        }
        // The client reports a user rejection as the app-side cancelled state.
        Some(request_computer_use_result::Result::Rejected(_)) => cancelled_json(),
        Some(request_computer_use_result::Result::Error(error)) => error_json(&error.message),
        None => missing_result_json("request_computer_use"),
    }
}

fn render_ask_user_question(result: &api::AskUserQuestionResult) -> String {
    match &result.result {
        Some(ask_user_question_result::Result::Success(success)) => {
            let answers = success
                .answers
                .iter()
                .map(|answer| match &answer.answer {
                    Some(answer_item::Answer::MultipleChoice(choice)) => json!({
                        "question_id": answer.question_id,
                        "status": "answered",
                        "selected_options": choice.selected_options,
                        "other_text": choice.other_text,
                    }),
                    Some(answer_item::Answer::Skipped(())) | None => json!({
                        "question_id": answer.question_id,
                        "status": "skipped",
                    }),
                })
                .collect::<Vec<_>>();
            json!({
                "status": "completed",
                "answers": answers,
            })
            .to_string()
        }
        Some(ask_user_question_result::Result::Error(error)) => error_json(&error.message),
        None => missing_result_json("ask_user_question"),
    }
}

fn render_run_agents(result: &api::RunAgentsResult) -> String {
    match &result.outcome {
        Some(run_agents_result::Outcome::Launched(launched)) => {
            let agents = launched
                .agents
                .iter()
                .map(agent_outcome_json)
                .collect::<Vec<_>>();
            json!({
                "status": "launched",
                "agents": agents,
                "instruction": "Child agents were launched. Do not re-run the same orchestration. Use launched agent_ids if follow-up is needed; otherwise continue with local tools.",
            })
            .to_string()
        }
        Some(run_agents_result::Outcome::Denied(denied)) => json!({
            "status": "denied",
            "error": denied.reason,
        })
        .to_string(),
        Some(run_agents_result::Outcome::Failure(failure)) => error_json(&failure.error),
        None => missing_result_json("run_agents"),
    }
}

fn agent_outcome_json(agent: &run_agents_result::AgentOutcome) -> Value {
    let mut value = match &agent.result {
        Some(agent_outcome::Result::Launched(launched)) => json!({
            "name": agent.name,
            "status": "launched",
            "agent_id": launched.agent_id,
        }),
        Some(agent_outcome::Result::Failed(failed)) => json!({
            "name": agent.name,
            "status": "failed",
            "error": failed.error,
        }),
        None => json!({
            "name": agent.name,
            "status": "unknown",
        }),
    };
    if !agent.model_id.is_empty() {
        value["model_id"] = Value::String(agent.model_id.clone());
    }
    if let Some(mode) = agent
        .execution_mode
        .as_ref()
        .and_then(|execution_mode| execution_mode.mode.as_ref())
    {
        value["execution_mode"] = execution_mode_json(mode);
    }
    value
}

fn execution_mode_json(mode: &ExecutionMode) -> Value {
    match mode {
        ExecutionMode::Local(_) => json!({ "type": "local" }),
        ExecutionMode::Remote(remote) => json!({
            "type": "remote",
            "environment_id": remote.environment_id,
            "worker_host": remote.worker_host,
            "computer_use_enabled": remote.computer_use_enabled,
            "runner_id": remote.runner_id,
        }),
    }
}

fn files_json(files: Vec<Value>, failed_reads: &[read_files_result::FailedRead]) -> String {
    let mut value = json!({ "files": files });
    if !failed_reads.is_empty() {
        value["failed_reads"] = failed_reads
            .iter()
            .map(|failed| json!({ "path": failed.path, "message": failed.message }))
            .collect();
    }
    value.to_string()
}

fn file_contents_to_json(files: impl Iterator<Item = Value>) -> String {
    json!({ "files": files.collect::<Vec<_>>() }).to_string()
}

fn file_content_json(file: &api::FileContent) -> Value {
    json!({
        "path": file.file_path,
        "content": file.content,
        "line_range": line_range_json(file.line_range.as_ref()),
        "line_count": file.content.lines().count(),
    })
}

fn any_file_content_json(file: &api::AnyFileContent) -> Option<Value> {
    match &file.content {
        Some(any_file_content::Content::TextContent(text)) => Some(file_content_json(text)),
        Some(any_file_content::Content::BinaryContent(binary)) => Some(json!({
            "path": binary.file_path,
            "content": "<binary content>",
            "line_range": Value::Null,
            "line_count": 0,
        })),
        None => None,
    }
}

fn line_range_json(range: Option<&api::FileContentLineRange>) -> Value {
    match range {
        Some(range) => json!({ "start": range.start, "end": range.end }),
        None => Value::Null,
    }
}

fn document_contents_to_json(status: &str, documents: &[api::DocumentContent]) -> String {
    let documents = documents
        .iter()
        .map(|document| {
            json!({
                "document_id": document.document_id,
                "content": document.content,
                "line_range": line_range_json(document.line_range.as_ref()),
            })
        })
        .collect::<Vec<_>>();
    json!({
        "status": status,
        "documents": documents,
    })
    .to_string()
}

fn mcp_resource_content_json(content: &api::McpResourceContent) -> Value {
    match &content.content_type {
        Some(mcp_resource_content::ContentType::Text(text)) => json!({
            "uri": content.uri,
            "mime_type": text.mime_type,
            "text": text.content,
        }),
        Some(mcp_resource_content::ContentType::Binary(binary)) => json!({
            "uri": content.uri,
            "mime_type": binary.mime_type,
            "blob": binary_placeholder(binary.data.len()),
        }),
        None => json!({ "uri": content.uri }),
    }
}

fn mcp_tool_content_json(item: &McpToolResultItem) -> Option<Value> {
    match &item.result {
        Some(McpToolContent::Text(text)) => Some(json!({ "type": "text", "text": text.text })),
        Some(McpToolContent::Image(image)) => Some(json!({
            "type": "image",
            "mime_type": image.mime_type,
            "data": binary_placeholder(image.data.len()),
        })),
        Some(McpToolContent::Resource(resource)) => Some(json!({
            "type": "resource",
            "resource": mcp_resource_content_json(resource),
        })),
        None => None,
    }
}

fn image_dimensions_json(image: Option<&api::RawImage>) -> Value {
    match image {
        Some(image) => json!({ "width": image.width, "height": image.height }),
        None => Value::Null,
    }
}

fn shell_command_error_message(error: &api::ShellCommandError) -> String {
    match error.r#type {
        Some(shell_command_error::Type::CommandNotFound(())) => {
            "CommandNotFound: no running command matches this block_id".to_string()
        }
        None => "Unknown shell command error".to_string(),
    }
}

fn shell_status(exit_code: i32) -> &'static str {
    if exit_code == 0 {
        "completed"
    } else {
        "failed"
    }
}

fn enum_name<E: TryFrom<i32> + std::fmt::Debug>(value: i32) -> String {
    match E::try_from(value) {
        Ok(variant) => format!("{variant:?}"),
        Err(_) => value.to_string(),
    }
}

fn binary_placeholder(len: usize) -> String {
    format!("<binary content: {len} bytes>")
}

fn error_json(message: &str) -> String {
    json!({ "status": "error", "error": message }).to_string()
}

fn bare_error_json(message: &str) -> String {
    json!({ "error": message }).to_string()
}

fn cancelled_json() -> String {
    json!({ "status": "cancelled" }).to_string()
}

fn missing_result_json(tool: &str) -> String {
    error_json(&format!("{tool} returned no result"))
}

#[cfg(test)]
#[path = "tool_results_tests.rs"]
mod tests;
