use std::collections::HashMap;
use std::sync::Arc;

use super::{ChatMessage, build_messages};
use crate::ai::agent::api::RequestParams;
use crate::ai::agent::{
    AIAgentActionResult, AIAgentActionResultType, AIAgentInput, InvokeSkillUserQuery,
    ReadFilesResult, UserQueryMode,
};

fn user_query(query: &str) -> AIAgentInput {
    AIAgentInput::UserQuery {
        query: query.to_string(),
        context: Arc::from([]),
        static_query_type: None,
        referenced_attachments: HashMap::new(),
        user_query_mode: UserQueryMode::Normal,
        running_command: None,
        intended_agent: None,
    }
}

fn user_roles(messages: &[ChatMessage]) -> Vec<&str> {
    messages
        .iter()
        .filter(|message| message.role == "user")
        .map(|message| message.content.as_str())
        .collect()
}

fn task_with_user_query(query: &str) -> warp_multi_agent_api::Task {
    warp_multi_agent_api::Task {
        id: "task-1".to_string(),
        messages: vec![warp_multi_agent_api::Message {
            fetched_memories: vec![],
            id: "msg-1".to_string(),
            task_id: "task-1".to_string(),
            server_message_data: String::new(),
            citations: vec![],
            message: Some(warp_multi_agent_api::message::Message::UserQuery(
                warp_multi_agent_api::message::UserQuery {
                    query: query.to_string(),
                    context: None,
                    referenced_attachments: HashMap::new(),
                    mode: None,
                    intended_agent: Default::default(),
                },
            )),
            request_id: "req-1".to_string(),
            timestamp: None,
        }],
        dependencies: None,
        description: String::new(),
        summary: String::new(),
        server_data: String::new(),
    }
}

fn action_result() -> AIAgentInput {
    AIAgentInput::ActionResult {
        result: AIAgentActionResult {
            id: "tool-1".to_string().into(),
            task_id: ai_types::TaskId::new("task-1".to_string()),
            result: AIAgentActionResultType::ReadFiles(ReadFilesResult::Cancelled),
        },
        context: Arc::from([]),
    }
}

#[test]
fn build_messages_includes_a_user_query() {
    let mut params = RequestParams::new_for_test();
    params.input = vec![user_query("where is main?")];

    let messages = build_messages(&params).expect("user query");

    assert!(messages.iter().any(|message| message.role == "system"));
    assert_eq!(user_roles(&messages), vec!["where is main?"]);
}

#[test]
fn build_messages_includes_skill_instructions() {
    let mut params = RequestParams::new_for_test();
    params.input = vec![AIAgentInput::InvokeSkill {
        context: Arc::from([]),
        skill: ai::skills::ParsedSkill {
            path: warp_util::local_or_remote_path::LocalOrRemotePath::Local(
                "/tmp/review-pr/SKILL.md".into(),
            ),
            name: "review-pr".to_string(),
            description: "Review a pull request.".to_string(),
            content: "Check the diff carefully.".to_string(),
            line_range: None,
            provider: ai::skills::SkillProvider::Agents,
            scope: ai::skills::SkillScope::Project,
        },
        user_query: Some(InvokeSkillUserQuery {
            query: "tighten the summary".to_string(),
            referenced_attachments: HashMap::new(),
        }),
    }];

    let messages = build_messages(&params).expect("skill turn");

    assert_eq!(
        user_roles(&messages),
        vec!["/review-pr tighten the summary\n\nCheck the diff carefully."]
    );
}

#[test]
fn build_messages_keeps_history_user_query_on_action_result_turns() {
    let mut params = RequestParams::new_for_test();
    params.tasks = vec![task_with_user_query("where is main?")];
    params.input = vec![action_result()];

    let messages = build_messages(&params).expect("continuation");

    assert_eq!(user_roles(&messages), vec!["where is main?"]);
}

#[test]
fn build_messages_rejects_a_request_with_no_user_query() {
    let params = RequestParams::new_for_test();

    let error = build_messages(&params).expect_err("system-only requests must not be sent");

    assert!(
        error
            .to_string()
            .contains("No user query found in messages.")
    );
}
