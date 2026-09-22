use local_agent_runtime::ToolCallResult;
use warp_multi_agent_api::request::settings::custom_model_providers::CustomEndpointSchema;

use super::*;
use crate::test_support::{
    TEST_BASE_URL, TEST_MODEL, agent_output_message, plan_mode, query_request, request, shell_call,
    shell_result, task, tool_call_message, tool_call_result, tool_call_result_message, user_query,
    user_query_message,
};

fn ids() -> TurnIds {
    TurnIds {
        conversation_id: "conversation_1".to_string(),
        request_id: "request_1".to_string(),
        run_id: "run_1".to_string(),
        task_id: "task_1".to_string(),
        task_exists: true,
    }
}

fn with_history(mut request: api::Request, messages: Vec<api::Message>) -> api::Request {
    request.task_context = Some(api::request::TaskContext {
        tasks: vec![task("task_1", messages)],
    });
    request
}

fn png_bytes() -> Vec<u8> {
    let mut bytes = Vec::new();
    image::RgbaImage::new(2, 2)
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .unwrap();
    bytes
}

fn with_images(mut request: api::Request, images: Vec<api::input_context::Image>) -> api::Request {
    request
        .input
        .as_mut()
        .unwrap()
        .context
        .get_or_insert_with(Default::default)
        .images = images;
    request
}

#[test]
fn turn_ids_come_from_metadata_and_tasks() {
    let mut request = with_history(
        query_request("hi"),
        vec![user_query_message("task_1", "earlier", None)],
    );
    request
        .task_context
        .as_mut()
        .unwrap()
        .tasks
        .push(task("task_2", vec![]));
    request.metadata.as_mut().unwrap().ambient_agent_task_id = "run_x".to_string();

    let ids = TurnIds::from_request(&request);
    assert_eq!(ids.conversation_id, "conversation_1");
    assert_eq!(ids.run_id, "run_x");
    assert_eq!(ids.task_id, "task_2");
    assert!(ids.task_exists);
    assert!(!ids.request_id.is_empty());

    let fresh = TurnIds::from_request(&api::Request::default());
    assert!(!fresh.conversation_id.is_empty());
    assert!(!fresh.run_id.is_empty());
    assert!(!fresh.task_id.is_empty());
    assert!(!fresh.task_exists);
}

#[test]
fn provider_is_selected_by_config_key() {
    let plan = plan_turn(&query_request("hi"), ids()).unwrap();
    assert_eq!(
        plan.provider,
        ProviderConfig {
            base_url: TEST_BASE_URL.to_string(),
            api_key: None,
            model: TEST_MODEL.to_string(),
        }
    );
    assert_eq!(plan.context_window_limit, None);

    let mut keyed = query_request("hi");
    let settings = keyed.settings.as_mut().unwrap();
    settings.custom_model_providers.as_mut().unwrap().providers[0].api_key = " secret ".to_string();
    settings
        .model_config
        .as_mut()
        .unwrap()
        .base_model_context_window_limit = 32_000;
    let plan = plan_turn(&keyed, ids()).unwrap();
    assert_eq!(plan.provider.api_key.as_deref(), Some("secret"));
    assert_eq!(plan.context_window_limit, Some(32_000));
}

#[test]
fn missing_or_unsupported_provider_is_an_error() {
    let mut other = query_request("hi");
    other
        .settings
        .as_mut()
        .unwrap()
        .model_config
        .as_mut()
        .unwrap()
        .base = "other".to_string();
    assert!(matches!(
        plan_turn(&other, ids()),
        Err(TurnPlanError::MissingProvider(base)) if base == "other"
    ));

    let mut anthropic = query_request("hi");
    anthropic
        .settings
        .as_mut()
        .unwrap()
        .custom_model_providers
        .as_mut()
        .unwrap()
        .providers[0]
        .schema = CustomEndpointSchema::AnthropicMessages as i32;
    assert!(matches!(
        plan_turn(&anthropic, ids()),
        Err(TurnPlanError::UnsupportedSchema(_))
    ));
}

#[test]
fn canonical_transcript_data_restores_exact_call_and_error_result() {
    let call = shell_call("call_1", "ls");
    let error = ToolCallResult::error(r#"{"error":"denied"}"#);
    let request = with_history(
        query_request("next"),
        vec![
            user_query_message("task_1", "List files", None),
            tool_call_message("task_1", &call, None),
            tool_call_result_message("task_1", "call_1", &error),
            agent_output_message("task_1", "Denied, sorry."),
        ],
    );

    let plan = plan_turn(&request, ids()).unwrap();

    assert_eq!(plan.initial_messages.len(), 4);
    let Message::Assistant(assistant) = &plan.initial_messages[1] else {
        panic!("expected the assistant tool call");
    };
    assert_eq!(assistant.tool_calls.len(), 1);
    assert_eq!(assistant.tool_calls[0].name, "run_shell_command");
    assert_eq!(assistant.tool_calls[0].arguments["command"], "ls");
    let Message::ToolResult(result) = &plan.initial_messages[2] else {
        panic!("expected the tool result");
    };
    assert_eq!(result.call_id, "call_1");
    assert!(result.result.is_error);
    assert_eq!(result.result.content, error.content);
    assert_eq!(plan.user_input.text_content(), "next");
    assert!(plan.echo_messages.is_empty());
}

#[test]
fn unpaired_tool_calls_and_results_are_removed_together() {
    let request = with_history(
        query_request("next"),
        vec![
            user_query_message("task_1", "List files", None),
            tool_call_message("task_1", &shell_call("call_lost", "ls"), None),
            tool_call_result_message("task_1", "call_orphan", &ToolCallResult::success("x")),
        ],
    );

    let plan = plan_turn(&request, ids()).unwrap();

    assert_eq!(plan.initial_messages.len(), 1);
    assert!(
        matches!(&plan.initial_messages[0], Message::User(user) if user.text_content() == "List files")
    );
}

#[test]
fn tool_result_continuation_uses_grounding_cue_and_echoes_result() {
    let call = shell_call("call_1", "ls");
    let request = with_history(
        request(vec![tool_call_result(
            "call_1",
            Some(shell_result("ls", "a\nb\n", 0)),
        )]),
        vec![
            user_query_message("task_1", "List files", None),
            tool_call_message("task_1", &call, None),
        ],
    );

    let plan = plan_turn(&request, ids()).unwrap();

    assert!(
        plan.user_input
            .text_content()
            .starts_with("Tool results are above and succeeded")
    );
    let Message::ToolResult(result) = plan.initial_messages.last().unwrap() else {
        panic!("expected the new tool result to end the history");
    };
    assert_eq!(result.call_id, "call_1");
    assert!(!result.result.is_error);
    assert!(result.result.content.contains("a\\nb"));

    assert_eq!(plan.echo_messages.len(), 1);
    let echoed = &plan.echo_messages[0];
    assert_eq!(echoed.task_id, "task_1");
    assert_eq!(echoed.request_id, "request_1");
    let Some(api::message::Message::ToolCallResult(echoed_result)) = &echoed.message else {
        panic!("expected a persisted ToolCallResult message");
    };
    assert_eq!(echoed_result.tool_call_id, "call_1");
    assert!(matches!(
        echoed_result.result,
        Some(api::message::tool_call_result::Result::RunShellCommand(_))
    ));
    let (call_id, decoded) =
        decode_local_runtime_tool_result_data(&echoed.server_message_data).unwrap();
    assert_eq!(call_id, "call_1");
    assert_eq!(decoded.content, result.result.content);

    let failed = with_history(
        crate::test_support::request(vec![tool_call_result(
            "call_1",
            Some(shell_result("ls", "boom", 2)),
        )]),
        vec![
            user_query_message("task_1", "List files", None),
            tool_call_message("task_1", &call, None),
        ],
    );
    let plan = plan_turn(&failed, ids()).unwrap();
    assert!(
        plan.user_input
            .text_content()
            .starts_with("Tool results are above (some failed)")
    );
}

#[test]
fn user_query_wins_over_grounding_cue() {
    let call = shell_call("call_1", "ls");
    let request = with_history(
        request(vec![
            tool_call_result("call_1", Some(shell_result("ls", "a\n", 0))),
            user_query("and then?", Some(plan_mode())),
        ]),
        vec![
            user_query_message("task_1", "List files", None),
            tool_call_message("task_1", &call, None),
        ],
    );

    let plan = plan_turn(&request, ids()).unwrap();

    assert_eq!(plan.user_input.text_content(), "and then?");
    assert_eq!(plan.echo_messages.len(), 1);
    assert_eq!(
        plan.registry.permission_mode(),
        crate::registry::LocalRuntimePermissionMode::Plan
    );
}

#[test]
fn missing_user_query_is_rejected() {
    assert!(matches!(
        plan_turn(&request(vec![]), ids()),
        Err(TurnPlanError::MissingUserQuery)
    ));
    assert!(matches!(
        plan_turn(&request(vec![user_query("   ", None)]), ids()),
        Err(TurnPlanError::MissingUserQuery)
    ));
}

#[test]
fn image_attachments_become_content_parts_capped_and_validated() {
    let text_only = plan_turn(&query_request("look"), ids()).unwrap();
    assert_eq!(text_only.user_input.parts.len(), 1);

    let mut images = vec![api::input_context::Image {
        data: b"not an image".to_vec(),
        mime_type: "image/png".to_string(),
    }];
    images.extend(
        (0..MAX_IMAGE_COUNT_FOR_QUERY + 1).map(|_| api::input_context::Image {
            data: png_bytes(),
            mime_type: "image/png".to_string(),
        }),
    );
    let plan = plan_turn(&with_images(query_request("look"), images), ids()).unwrap();

    let parts = &plan.user_input.parts;
    assert_eq!(parts.len(), 1 + MAX_IMAGE_COUNT_FOR_QUERY);
    assert!(matches!(&parts[0], ContentPart::Text(text) if text == "look"));
    assert!(parts[1..].iter().all(|part| matches!(
        part,
        ContentPart::Image { mime_type, data_base64, file_name: None }
            if mime_type == "image/png" && !data_base64.is_empty()
    )));
    assert!(plan.user_input.has_images());
}
