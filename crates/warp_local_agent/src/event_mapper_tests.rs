use super::*;

fn client_actions(event: &api::ResponseEvent) -> &[api::ClientAction] {
    let Some(api::response_event::Type::ClientActions(client_actions)) = &event.r#type else {
        panic!("expected client actions event");
    };
    &client_actions.actions
}

fn mapper() -> EventMapper {
    EventMapper::new(
        "conversation_1".to_string(),
        "request_1".to_string(),
        "run_1".to_string(),
        "task_1".to_string(),
        true,
        Arc::new(LocalRuntimeToolRegistry::built_ins()),
    )
}

fn tool_call_ids(events: &[api::ResponseEvent]) -> Vec<String> {
    events
        .iter()
        .flat_map(|event| client_actions(event).iter())
        .filter_map(|action| match &action.action {
            Some(api::client_action::Action::AddMessagesToTask(add)) => Some(&add.messages),
            _ => None,
        })
        .flatten()
        .filter_map(|message| match &message.message {
            Some(api::message::Message::ToolCall(call)) => Some(call.tool_call_id.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn text_delta_creates_then_appends_agent_output_message() {
    let mut mapper = mapper();

    let first = mapper.map_event(&RuntimeEvent::TextDelta {
        text: "hel".to_string(),
    });
    let second = mapper.map_event(&RuntimeEvent::TextDelta {
        text: "lo".to_string(),
    });

    let first_actions = client_actions(&first[0]);
    assert_eq!(first_actions.len(), 3);
    let Some(api::client_action::Action::AddMessagesToTask(add)) = &first_actions[1].action else {
        panic!("expected AddMessagesToTask action");
    };
    let message_id = add.messages[0].id.clone();
    let Some(api::message::Message::AgentOutput(output)) = &add.messages[0].message else {
        panic!("expected agent output message");
    };
    assert_eq!(output.text, "hel");

    let second_actions = client_actions(&second[0]);
    assert_eq!(second_actions.len(), 3);
    let Some(api::client_action::Action::AppendToMessageContent(append)) =
        &second_actions[1].action
    else {
        panic!("expected AppendToMessageContent action");
    };
    assert_eq!(append.task_id, "task_1");
    assert_eq!(
        append.mask.as_ref().unwrap().paths,
        vec!["agent_output.text"]
    );

    let message = append.message.as_ref().unwrap();
    assert_eq!(message.id, message_id);
    let Some(api::message::Message::AgentOutput(output)) = &message.message else {
        panic!("expected agent output message");
    };
    assert_eq!(output.text, "lo");

    let merged = field_mask::FieldMaskOperation::append(
        &api::MESSAGE_DESCRIPTOR,
        &add.messages[0],
        message,
        append.mask.clone().unwrap(),
    )
    .apply()
    .expect("append field mask should apply");
    let Some(api::message::Message::AgentOutput(output)) = &merged.message else {
        panic!("expected merged agent output message");
    };
    assert_eq!(output.text, "hello");
}

#[test]
fn edit_files_tool_call_maps_to_apply_file_diffs_client_action() {
    let mut mapper = mapper();

    let events = mapper.map_event(&RuntimeEvent::ToolCallsDeferred {
        calls: vec![ToolCall {
            id: "call_1".to_string(),
            name: "edit_files".to_string(),
            arguments: serde_json::json!({
                "title": "Update greeting",
                "edits": [
                    {
                        "type": "replace",
                        "file": "/tmp/warp-agent-easy/hello.rs",
                        "search": "println!(\"Hello\");",
                        "replace": "println!(\"Hello from the local agent!\");"
                    }
                ]
            }),
        }],
    });

    let actions = client_actions(&events[0]);
    let Some(api::client_action::Action::AddMessagesToTask(add)) = &actions[1].action else {
        panic!("expected AddMessagesToTask action");
    };
    let Some(api::message::Message::ToolCall(tool_call)) = &add.messages[0].message else {
        panic!("expected tool call message");
    };
    let Some(api::message::tool_call::Tool::ApplyFileDiffs(diff)) = &tool_call.tool else {
        panic!("expected ApplyFileDiffs tool");
    };

    assert_eq!(tool_call.tool_call_id, "call_1");
    assert_eq!(diff.summary, "Update greeting");
    assert_eq!(diff.diffs.len(), 1);
    assert_eq!(diff.diffs[0].file_path, "/tmp/warp-agent-easy/hello.rs");
}

#[test]
fn client_tool_calls_are_persisted_when_deferred_not_when_requested() {
    // Todo tools only exist on request-built registries.
    let mut mapper = EventMapper::new(
        "conversation_1".to_string(),
        "request_1".to_string(),
        "run_1".to_string(),
        "task_1".to_string(),
        true,
        Arc::new(LocalRuntimeToolRegistry::from_request(
            &api::Request::default(),
        )),
    );
    let shell = ToolCall {
        id: "call_shell".to_string(),
        name: "run_shell_command".to_string(),
        arguments: serde_json::json!({ "command": "ls" }),
    };
    let todo = ToolCall {
        id: "call_todo".to_string(),
        name: "update_todos".to_string(),
        arguments: serde_json::json!({ "operation": "create", "todos": [] }),
    };

    let requested = mapper.map_event(&RuntimeEvent::ToolCallsRequested {
        calls: vec![shell.clone(), todo],
    });
    assert_eq!(tool_call_ids(&requested), vec!["call_todo"]);

    let deferred = mapper.map_event(&RuntimeEvent::ToolCallsDeferred { calls: vec![shell] });
    assert_eq!(tool_call_ids(&deferred), vec!["call_shell"]);
}

#[test]
fn awaiting_client_tool_results_finishes_the_stream_as_done() {
    let mut mapper = mapper();
    let events = mapper.map_event(&RuntimeEvent::Finished {
        reason: FinishReason::AwaitingClientToolResults,
    });
    assert!(matches!(
        &events[0].r#type,
        Some(api::response_event::Type::Finished(finished))
            if matches!(finished.reason, Some(api::response_event::stream_finished::Reason::Done(_)))
    ));
}

#[test]
fn init_is_emitted_once_even_when_the_caller_sent_it() {
    let mut mapper = mapper();
    mapper.mark_init_sent();
    assert!(
        mapper
            .map_event(&RuntimeEvent::TurnStarted { turn: 1 })
            .is_empty()
    );

    let mut fresh = mapper_without_init();
    let events = fresh.map_event(&RuntimeEvent::TurnStarted { turn: 1 });
    assert!(matches!(
        &events[0].r#type,
        Some(api::response_event::Type::Init(init)) if init.run_id == "run_1"
    ));
    assert!(
        fresh
            .map_event(&RuntimeEvent::TurnStarted { turn: 2 })
            .is_empty()
    );
}

fn mapper_without_init() -> EventMapper {
    mapper()
}
