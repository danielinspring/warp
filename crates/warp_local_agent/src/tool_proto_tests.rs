use std::collections::HashMap;

use local_agent_runtime::ToolSafetyClass;
use uuid::Uuid;

use super::*;
use crate::test_support::query_request;

fn call(name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: "call_1".to_string(),
        name: name.to_string(),
        arguments,
    }
}

fn assert_invalid_input(call: ToolCall, expected_reason: &str) {
    let err = tool_call_to_proto_tool(&call).unwrap_err();

    let ToolExecutionError::InvalidInput { reason } = err else {
        panic!("expected invalid input");
    };
    assert!(
        reason.contains(expected_reason),
        "expected `{reason}` to contain `{expected_reason}`"
    );
}

fn registry_with(tools: &[api::ToolType]) -> LocalRuntimeToolRegistry {
    let mut request = query_request("go");
    request
        .settings
        .get_or_insert_with(Default::default)
        .supported_tools = tools.iter().map(|tool| *tool as i32).collect();
    LocalRuntimeToolRegistry::from_request(&request)
}

fn round_trip(call: &ToolCall, registry: &LocalRuntimeToolRegistry) -> ToolCall {
    let proto_tool = tool_call_to_proto_tool_with_registry(call, registry).unwrap();
    let proto_call = api::message::ToolCall {
        tool_call_id: call.id.clone(),
        tool: Some(proto_tool),
    };
    proto_tool_call_to_runtime_with_registry(&proto_call, registry).unwrap()
}

#[test]
fn infer_shell_command_is_read_only_for_find_and_ls() {
    assert!(infer_shell_command_is_read_only(
        r#"find ~ -type d -name "learn-harness-engineering" 2>/dev/null"#
    ));
    assert!(infer_shell_command_is_read_only(
        r#"mdfind 'kMDItemFSName == "learn-harness-engineering"c' | head -20"#
    ));
    assert!(infer_shell_command_is_read_only(
        "cd ~ && ls -la | grep -i learn"
    ));
    assert!(infer_shell_command_is_read_only(
        r#"python3 -c "print(sum(range(1, 101)))""#
    ));
    assert!(infer_shell_command_is_read_only(
        r#"python -c "print(100*101//2)""#
    ));
    assert!(!infer_shell_command_is_read_only("python3 script.py"));
    assert!(!infer_shell_command_is_read_only("rm -rf /tmp/foo"));
    assert!(!infer_shell_command_is_read_only("echo hi > out.txt"));
}

#[test]
fn shell_command_is_read_only_respects_explicit_flag() {
    assert!(!shell_command_is_read_only(&serde_json::json!({
        "command": "find ~ -type d -name foo",
        "is_read_only": false,
    })));
    assert!(shell_command_is_read_only(&serde_json::json!({
        "command": "find ~ -type d -name foo",
    })));
}

#[test]
fn skill_ref_parses_and_renders_both_forms() {
    assert_eq!(
        SkillRef::parse("@warp-skill:review"),
        SkillRef::BundledSkillId("review".to_string())
    );
    assert_eq!(
        SkillRef::parse("/repo/.agents/skills/review/SKILL.md"),
        SkillRef::Path("/repo/.agents/skills/review/SKILL.md".to_string())
    );
    assert_eq!(
        SkillRef::BundledSkillId("review".to_string()).reference_string(),
        "@warp-skill:review"
    );
    assert_eq!(
        SkillRef::from_descriptor(&api::skill_descriptor::SkillReference::Path(
            "/x/SKILL.md".to_string()
        )),
        SkillRef::Path("/x/SKILL.md".to_string())
    );
}

#[test]
fn registry_proto_round_trip_preserves_generated_mcp_name_and_arguments() {
    let server_id = Uuid::new_v4();
    let function_name = "mcp__github__search_issues";
    let mut registry = LocalRuntimeToolRegistry::built_ins();
    registry.add_tool(
        ToolSchema {
            name: function_name.to_string(),
            description: "Search issues".to_string(),
            parameters: serde_json::json!({ "type": "object" }),
        },
        ToolSafetyClass::Interactive,
        LocalRuntimeToolRouteKind::McpTool {
            server_id: Some(server_id),
            name: "search_issues".to_string(),
        },
    );
    let call = ToolCall {
        id: "call_1".to_string(),
        name: function_name.to_string(),
        arguments: serde_json::json!({
            "query": "is:open",
            "labels": ["bug", "agent"],
        }),
    };

    let restored = round_trip(&call, &registry);

    assert_eq!(restored.id, call.id);
    assert_eq!(restored.name, call.name);
    assert_eq!(restored.arguments, call.arguments);
}

#[test]
fn registry_proto_round_trip_preserves_skill_reference() {
    let skill = "@warp-skill:review".to_string();
    let mut registry = LocalRuntimeToolRegistry::built_ins();
    registry.add_read_skill(HashMap::from([(
        skill.clone(),
        SkillRef::BundledSkillId("review".to_string()),
    )]));
    let call = call("read_skill", serde_json::json!({ "skill": skill }));

    let restored = round_trip(&call, &registry);

    assert_eq!(restored.name, "read_skill");
    assert_eq!(restored.arguments["skill"], "@warp-skill:review");
}

#[test]
fn background_shell_tools_map_and_round_trip() {
    use api::message::tool_call::Tool;
    use api::message::tool_call::write_to_long_running_shell_command::mode::Mode as ModeVariant;

    let registry = LocalRuntimeToolRegistry::built_ins();

    let read = call(
        "read_shell_command_output",
        serde_json::json!({ "command_id": "blk_1", "wait_until_complete": true }),
    );
    let Tool::ReadShellCommandOutput(proto) = tool_call_to_proto_tool(&read).unwrap() else {
        panic!("expected ReadShellCommandOutput");
    };
    assert_eq!(proto.command_id, "blk_1");
    assert!(matches!(
        proto.delay,
        Some(api::message::tool_call::read_shell_command_output::Delay::OnCompletion(()))
    ));
    let restored = round_trip(&read, &registry);
    assert_eq!(restored.arguments["block_id"], "blk_1");
    assert_eq!(restored.arguments["wait_until_complete"], true);

    let write = call(
        "write_to_long_running_shell_command",
        serde_json::json!({ "block_id": "blk_1", "input": "ls\n", "mode": "LINE" }),
    );
    let Tool::WriteToLongRunningShellCommand(proto) = tool_call_to_proto_tool(&write).unwrap()
    else {
        panic!("expected WriteToLongRunningShellCommand");
    };
    assert_eq!(proto.command_id, "blk_1");
    assert_eq!(proto.input, b"ls\n");
    assert!(matches!(
        proto.mode.as_ref().and_then(|mode| mode.mode.as_ref()),
        Some(ModeVariant::Line(()))
    ));
    let restored = round_trip(&write, &registry);
    assert_eq!(restored.arguments["mode"], "line");
    assert_eq!(restored.arguments["input"], "ls\n");

    assert_invalid_input(
        call(
            "write_to_long_running_shell_command",
            serde_json::json!({ "block_id": "blk_1", "input": "x", "mode": "paste" }),
        ),
        "mode must be raw, line, or block",
    );
    assert_invalid_input(
        call("read_shell_command_output", serde_json::json!({})),
        "requires one of: block_id or command_id",
    );
}

#[test]
fn run_agents_maps_to_local_proto_with_child_bounds() {
    use api::message::tool_call::Tool;

    let valid = call(
        "run_agents",
        serde_json::json!({
            "summary": "Split the work",
            "base_prompt": "Be brief.",
            "agents": [
                { "name": "docs", "prompt": "Write docs", "title": "Docs" },
                { "name": "tests", "prompt": "Write tests" }
            ]
        }),
    );
    let Tool::RunAgents(proto) = tool_call_to_proto_tool(&valid).unwrap() else {
        panic!("expected RunAgents");
    };
    assert_eq!(proto.summary, "Split the work");
    assert_eq!(proto.base_prompt, "Be brief.");
    assert_eq!(proto.agent_run_configs.len(), 2);
    assert_eq!(proto.agent_run_configs[0].title, "Docs");
    assert!(proto.agent_run_configs[1].title.is_empty());
    assert!(matches!(
        proto.execution_mode,
        Some(api::run_agents::ExecutionModeOneOf::Local(_))
    ));

    let restored = round_trip(&valid, &registry_with(&[api::ToolType::RunAgents]));
    assert_eq!(restored.arguments["summary"], "Split the work");
    assert_eq!(restored.arguments["agents"][0]["title"], "Docs");
    assert!(restored.arguments["agents"][1].get("title").is_none());

    let too_many = (0..LOCAL_RUN_AGENTS_MAX_CHILDREN + 1)
        .map(|index| serde_json::json!({ "name": format!("agent_{index}"), "prompt": "go" }))
        .collect::<Vec<_>>();
    assert_invalid_input(
        call(
            "run_agents",
            serde_json::json!({ "summary": "too many", "agents": too_many }),
        ),
        "allows at most 4 child agents",
    );
    assert_invalid_input(
        call(
            "run_agents",
            serde_json::json!({ "summary": "none", "agents": [] }),
        ),
        "requires at least one agent",
    );
}

#[test]
fn ask_user_question_maps_to_proto_and_round_trips() {
    use api::ask_user_question::question::QuestionType;
    use api::message::tool_call::Tool;

    let valid = call(
        "ask_user_question",
        serde_json::json!({
            "questions": [{
                "question_id": "q1",
                "question": "Which one?",
                "options": ["A", "B", "C"],
                "recommended_option_index": 1,
                "is_multiselect": true
            }]
        }),
    );
    let Tool::AskUserQuestion(proto) = tool_call_to_proto_tool(&valid).unwrap() else {
        panic!("expected AskUserQuestion");
    };
    let Some(QuestionType::MultipleChoice(choice)) = &proto.questions[0].question_type else {
        panic!("expected multiple choice");
    };
    assert_eq!(choice.options.len(), 3);
    assert_eq!(choice.recommended_option_index, 1);
    assert!(choice.is_multiselect);
    assert!(choice.supports_other);

    let registry = registry_with(&[api::ToolType::AskUserQuestion]);
    let restored = round_trip(&valid, &registry);
    assert_eq!(restored.arguments["questions"][0]["options"][2], "C");
    assert_eq!(
        restored.arguments["questions"][0]["recommended_option_index"],
        1
    );

    assert_invalid_input(
        call(
            "ask_user_question",
            serde_json::json!({ "questions": [{ "question_id": "q", "question": "?", "options": ["only"] }] }),
        ),
        "at least two options",
    );
}

#[test]
fn schemas_advertise_only_supported_v1_tools() {
    let schemas = build_tool_schemas();
    let names = schemas
        .iter()
        .map(|schema| schema.name.as_str())
        .collect::<Vec<_>>();

    assert_eq!(
        names,
        vec![
            "run_shell_command",
            "read_shell_command_output",
            "write_to_long_running_shell_command",
            "read_files",
            "grep",
            "file_glob_v2",
            "search_codebase",
            "edit_files"
        ]
    );
    assert!(!names.contains(&"create_file"));
    assert!(
        schemas
            .iter()
            .all(|schema| schema.parameters["additionalProperties"] == false)
    );
}

#[test]
fn schemas_keep_arguments_conservative() {
    let schemas = build_tool_schemas()
        .into_iter()
        .map(|schema| (schema.name.clone(), schema))
        .collect::<HashMap<_, _>>();

    assert_eq!(
        schemas["run_shell_command"].parameters["required"],
        serde_json::json!(["command"])
    );
    assert_eq!(
        schemas["read_files"].parameters["required"],
        serde_json::json!(["paths"])
    );
    assert_eq!(
        schemas["grep"].parameters["required"],
        serde_json::json!(["queries"])
    );
    assert_eq!(
        schemas["file_glob_v2"].parameters["required"],
        serde_json::json!(["patterns"])
    );
    assert_eq!(
        schemas["search_codebase"].parameters["required"],
        serde_json::json!(["query"])
    );
    assert_eq!(
        schemas["edit_files"].parameters["required"],
        serde_json::json!(["edits"])
    );
}

#[test]
fn tool_calls_reject_non_object_arguments() {
    assert_invalid_input(
        call("read_files", serde_json::json!("src/lib.rs")),
        "requires object arguments",
    );
}

#[test]
fn tool_calls_reject_unsupported_arguments() {
    assert_invalid_input(
        call(
            "run_shell_command",
            serde_json::json!({"command": "pwd", "cwd": "/tmp"}),
        ),
        "does not support argument",
    );
}

#[test]
fn tool_calls_reject_wrong_argument_types() {
    let cases = [
        (
            call("run_shell_command", serde_json::json!({"command": 1})),
            "command` must be a string",
        ),
        (
            call(
                "run_shell_command",
                serde_json::json!({"command": "pwd", "is_read_only": "yes"}),
            ),
            "is_read_only` must be a boolean",
        ),
        (
            call(
                "read_files",
                serde_json::json!({"paths": ["src/lib.rs", 1]}),
            ),
            "paths` must contain only strings",
        ),
        (
            call("grep", serde_json::json!({"queries": []})),
            "requires non-empty string array argument `queries`",
        ),
        (
            call("file_glob_v2", serde_json::json!({"pattern": ""})),
            "pattern` must be a non-empty string",
        ),
        (
            call(
                "search_codebase",
                serde_json::json!({"query": "runtime loop", "path_filters": ["app", false]}),
            ),
            "path_filters` must contain only strings",
        ),
    ];

    for (call, expected_reason) in cases {
        assert_invalid_input(call, expected_reason);
    }
}

#[test]
fn tool_calls_reject_empty_required_values() {
    let cases = [
        (
            call("run_shell_command", serde_json::json!({"command": "  "})),
            "command` must be a non-empty string",
        ),
        (
            call("read_files", serde_json::json!({"paths": []})),
            "requires non-empty string array argument `paths`",
        ),
        (
            call("search_codebase", serde_json::json!({"query": ""})),
            "query` must be a non-empty string",
        ),
    ];

    for (call, expected_reason) in cases {
        assert_invalid_input(call, expected_reason);
    }
}

#[test]
fn tool_calls_reject_invalid_preferred_alias_instead_of_falling_back() {
    assert_invalid_input(
        call(
            "grep",
            serde_json::json!({"queries": 1, "pattern": "needle"}),
        ),
        "queries` must be a string array",
    );
    assert_invalid_input(
        call(
            "file_glob_v2",
            serde_json::json!({"patterns": 1, "pattern": "**/*.rs"}),
        ),
        "patterns` must be a string array",
    );
}

#[test]
fn tool_calls_accept_stringified_string_arrays() {
    let proto = tool_call_to_proto_tool(&call(
        "read_files",
        serde_json::json!({"paths": "[\"src/lib.rs\"]"}),
    ))
    .unwrap();

    let api::message::tool_call::Tool::ReadFiles(read_files) = proto else {
        panic!("expected ReadFiles tool call");
    };
    assert_eq!(read_files.files.len(), 1);
    assert_eq!(read_files.files[0].name, "src/lib.rs");
}

#[test]
fn tool_calls_reject_stringified_arrays_with_non_strings() {
    assert_invalid_input(
        call(
            "read_files",
            serde_json::json!({"paths": "[\"src/lib.rs\", 1]"}),
        ),
        "paths` must contain only strings",
    );
}

#[test]
fn supported_tool_names_match_advertised_schema_names() {
    let registry = LocalRuntimeToolRegistry::built_ins();
    let advertised_names = build_tool_schemas()
        .into_iter()
        .map(|schema| schema.name)
        .collect::<Vec<_>>();

    for name in &advertised_names {
        assert!(registry.contains_tool(name));
    }

    assert_eq!(
        advertised_names,
        vec![
            "run_shell_command",
            "read_shell_command_output",
            "write_to_long_running_shell_command",
            "read_files",
            "grep",
            "file_glob_v2",
            "search_codebase",
            "edit_files"
        ]
    );
}

#[test]
fn unsupported_tool_is_not_silently_mapped() {
    let err = tool_call_to_proto_tool(&call(
        "write_file",
        serde_json::json!({"path": "src/lib.rs"}),
    ))
    .unwrap_err();

    assert!(matches!(err, ToolExecutionError::NotFound { name } if name == "write_file"));
}

#[test]
fn edit_files_maps_to_apply_file_diffs_proto_for_ui_rendering() {
    let proto = tool_call_to_proto_tool(&call(
        "edit_files",
        serde_json::json!({
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
    ))
    .unwrap();

    let api::message::tool_call::Tool::ApplyFileDiffs(diff) = proto else {
        panic!("expected ApplyFileDiffs tool call");
    };

    assert_eq!(diff.summary, "Update greeting");
    assert_eq!(diff.diffs.len(), 1);
    assert_eq!(diff.diffs[0].file_path, "/tmp/warp-agent-easy/hello.rs");
    assert_eq!(diff.diffs[0].search, "println!(\"Hello\");");
    assert_eq!(
        diff.diffs[0].replace,
        "println!(\"Hello from the local agent!\");"
    );
    assert!(diff.new_files.is_empty());
    assert!(diff.deleted_files.is_empty());
    assert!(diff.v4a_updates.is_empty());
}
