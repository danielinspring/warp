use local_agent_runtime::decode_local_runtime_tool_result_data;

use super::*;
use crate::test_support::shell_result;

fn result(id: &str, result: Option<RequestResult>) -> api::request::input::ToolCallResult {
    api::request::input::ToolCallResult {
        tool_call_id: id.to_string(),
        result,
    }
}

fn read_files_success(path: &str, content: &str) -> RequestResult {
    RequestResult::ReadFiles(api::ReadFilesResult {
        result: Some(read_files_result::Result::TextFilesSuccess(
            read_files_result::TextFilesSuccess {
                files: vec![api::FileContent {
                    file_path: path.to_string(),
                    content: content.to_string(),
                    line_range: None,
                }],
                failed_reads: vec![],
            },
        )),
    })
}

#[test]
fn shell_action_results_serialize_long_running_and_cancelled_for_model() {
    let long_running = RequestResult::RunShellCommand(api::RunShellCommandResult {
        command: "find ~ -type d -name foo".to_string(),
        result: Some(
            run_shell_command_result::Result::LongRunningCommandSnapshot(
                api::LongRunningShellCommandSnapshot {
                    command_id: "blk_1".to_string(),
                    output: "/Users/me/foo\n".to_string(),
                    ..Default::default()
                },
            ),
        ),
        ..Default::default()
    });
    let rendered = render_tool_call_result(&result("call_1", Some(long_running)));
    assert!(!rendered.is_error);
    assert!(rendered.content.contains("\"status\":\"long_running\""));
    assert!(rendered.content.contains("/Users/me/foo"));
    assert!(rendered.content.contains("\"block_id\":\"blk_1\""));

    let never_ran = RequestResult::RunShellCommand(api::RunShellCommandResult {
        command: "ls".to_string(),
        result: None,
        ..Default::default()
    });
    let rendered = render_tool_call_result(&result("call_1", Some(never_ran)));
    assert!(rendered.is_error);
    assert!(rendered.content.contains("\"status\":\"cancelled\""));
    assert!(rendered.content.contains("previous command"));
}

#[test]
fn completed_shell_result_puts_stdout_and_forbids_invented_timeout() {
    let completed = shell_result(r#"python3 -c "print(sum(range(1, 101)))""#, "5050\n", 0);
    let rendered = render_tool_call_result(&result("call_1", Some(completed)));
    assert!(!rendered.is_error);
    assert!(rendered.content.contains("\"status\":\"completed\""));
    assert!(rendered.content.contains("5050"));
    assert!(rendered.content.contains("\"stdout\""));
    assert!(rendered.content.contains("Do not invent timeouts"));

    let failed = shell_result("false", "", 1);
    let rendered = render_tool_call_result(&result("call_1", Some(failed)));
    assert!(rendered.is_error);
    assert!(rendered.content.contains("\"status\":\"failed\""));
    assert!(rendered.content.contains("\"exit_code\":1"));
}

#[test]
fn ask_user_question_result_preserves_answer_skip_and_cancel_states() {
    let answered = RequestResult::AskUserQuestion(api::AskUserQuestionResult {
        result: Some(ask_user_question_result::Result::Success(
            ask_user_question_result::Success {
                answers: vec![
                    ask_user_question_result::AnswerItem {
                        question_id: "one".to_string(),
                        answer: Some(answer_item::Answer::MultipleChoice(
                            answer_item::MultipleChoiceAnswer {
                                selected_options: vec!["A".to_string()],
                                other_text: String::new(),
                            },
                        )),
                    },
                    ask_user_question_result::AnswerItem {
                        question_id: "two".to_string(),
                        answer: Some(answer_item::Answer::Skipped(())),
                    },
                ],
            },
        )),
    });
    let rendered = render_tool_call_result(&result("call_1", Some(answered)));
    assert!(!rendered.is_error);
    assert!(rendered.content.contains("\"status\":\"answered\""));
    assert!(rendered.content.contains("\"status\":\"skipped\""));
    assert!(rendered.content.contains("\"selected_options\":[\"A\"]"));

    let cancelled = render_tool_call_result(&result("call_1", None));
    assert!(cancelled.is_error);
    assert_eq!(cancelled.content, "Tool call cancelled");
}

#[test]
fn run_agents_result_content_covers_launch_denial_and_failure() {
    let launched = RequestResult::RunAgentsResult(api::RunAgentsResult {
        outcome: Some(run_agents_result::Outcome::Launched(
            run_agents_result::Launched {
                agents: vec![run_agents_result::AgentOutcome {
                    name: "docs".to_string(),
                    result: Some(agent_outcome::Result::Launched(
                        run_agents_result::LaunchedAgent {
                            agent_id: "agent_1".to_string(),
                        },
                    )),
                    ..Default::default()
                }],
                ..Default::default()
            },
        )),
    });
    let rendered = render_tool_call_result(&result("call_1", Some(launched)));
    assert!(!rendered.is_error);
    assert!(rendered.content.contains("\"status\":\"launched\""));
    assert!(rendered.content.contains("agent_1"));
    assert!(
        rendered
            .content
            .contains("Do not re-run the same orchestration")
    );

    let denied = RequestResult::RunAgentsResult(api::RunAgentsResult {
        outcome: Some(run_agents_result::Outcome::Denied(
            run_agents_result::Denied {
                reason: "user declined".to_string(),
            },
        )),
    });
    let rendered = render_tool_call_result(&result("call_1", Some(denied)));
    assert!(rendered.is_error);
    assert!(rendered.content.contains("\"status\":\"denied\""));
    assert!(rendered.content.contains("user declined"));

    let failed = RequestResult::RunAgentsResult(api::RunAgentsResult {
        outcome: Some(run_agents_result::Outcome::Failure(
            run_agents_result::Failure {
                error: "orchestrator offline".to_string(),
            },
        )),
    });
    let rendered = render_tool_call_result(&result("call_1", Some(failed)));
    assert!(rendered.is_error);
    assert!(rendered.content.contains("orchestrator offline"));
}

#[test]
fn read_files_content_includes_real_file_content() {
    let rendered = render_tool_call_result(&result(
        "call_1",
        Some(read_files_success("src/lib.rs", "fn main() {}")),
    ));
    assert!(!rendered.is_error);
    assert!(rendered.content.contains("src/lib.rs"));
    assert!(rendered.content.contains("fn main() {}"));

    let errored = RequestResult::ReadFiles(api::ReadFilesResult {
        result: Some(read_files_result::Result::Error(read_files_result::Error {
            message: "permission denied".to_string(),
        })),
    });
    let rendered = render_tool_call_result(&result("call_1", Some(errored)));
    assert!(rendered.is_error);
    assert!(rendered.content.contains("permission denied"));
}

#[test]
fn tool_call_result_message_persists_typed_result_and_envelope() {
    let proto_result = result(
        "call_1",
        Some(read_files_success("src/lib.rs", "fn main() {}")),
    );
    let rendered = render_tool_call_result(&proto_result);

    let message = tool_call_result_message("task_1", "request_1", &proto_result, &rendered);

    assert_eq!(message.task_id, "task_1");
    assert_eq!(message.request_id, "request_1");
    assert!(!message.id.is_empty());
    let Some(api::message::Message::ToolCallResult(persisted)) = &message.message else {
        panic!("expected persisted ToolCallResult message");
    };
    assert_eq!(persisted.tool_call_id, "call_1");
    assert!(matches!(
        &persisted.result,
        Some(api::message::tool_call_result::Result::ReadFiles(_))
    ));
    let (call_id, decoded) =
        decode_local_runtime_tool_result_data(&message.server_message_data).unwrap();
    assert_eq!(call_id, "call_1");
    assert_eq!(decoded.content, rendered.content);
    assert!(!decoded.is_error);
}

#[test]
fn cancelled_result_persists_cancel_marker() {
    let proto_result = result("call_1", None);
    let rendered = render_tool_call_result(&proto_result);

    let message = tool_call_result_message("task_1", "request_1", &proto_result, &rendered);

    let Some(api::message::Message::ToolCallResult(persisted)) = &message.message else {
        panic!("expected persisted ToolCallResult message");
    };
    assert_eq!(persisted.tool_call_id, "call_1");
    assert!(matches!(
        &persisted.result,
        Some(api::message::tool_call_result::Result::Cancel(()))
    ));
    let (_, decoded) = decode_local_runtime_tool_result_data(&message.server_message_data).unwrap();
    assert!(decoded.is_error);
    assert_eq!(decoded.content, "Tool call cancelled");
}

#[test]
fn to_message_result_round_trips_read_files_payload() {
    let RequestResult::ReadFiles(payload) = read_files_success("a.rs", "x") else {
        unreachable!();
    };
    let MessageResult::ReadFiles(persisted) =
        to_message_result(RequestResult::ReadFiles(payload.clone()))
    else {
        panic!("expected ReadFiles to stay ReadFiles");
    };
    assert_eq!(persisted, payload);
}
