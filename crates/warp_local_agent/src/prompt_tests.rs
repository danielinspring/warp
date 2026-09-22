use super::*;

fn request_fixture() -> api::Request {
    api::Request {
        input: Some(api::request::Input {
            context: Some(api::InputContext {
                directory: Some(api::input_context::Directory {
                    pwd: "/repo/warp".to_string(),
                    home: "/Users/me".to_string(),
                    pwd_file_symbols_indexed: true,
                }),
                shell: Some(api::input_context::Shell {
                    name: "zsh".to_string(),
                    version: "5.9".to_string(),
                }),
                ..Default::default()
            }),
            r#type: None,
        }),
        settings: Some(api::request::Settings {
            rules_enabled: true,
            warp_drive_context_enabled: true,
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn context_of(request: &api::Request) -> &api::InputContext {
    request
        .input
        .as_ref()
        .and_then(|input| input.context.as_ref())
        .expect("fixture has an input context")
}

fn context_of_mut(request: &mut api::Request) -> &mut api::InputContext {
    request
        .input
        .as_mut()
        .and_then(|input| input.context.as_mut())
        .expect("fixture has an input context")
}

fn image_fixture() -> api::input_context::Image {
    api::input_context::Image {
        data: vec![0; 128],
        mime_type: "image/png".to_string(),
    }
}

#[test]
fn request_prompt_includes_pwd_and_shell_lines() {
    let request = request_fixture();
    let prompt = format_system_prompt(&PromptBuildInput::from_request_context(&request, false));

    assert!(prompt.contains(SYSTEM_PROMPT));
    assert!(prompt.contains("- Working directory: /repo/warp\n"));
    assert!(prompt.contains("- Shell: zsh 5.9\n"));
    assert!(
        prompt.contains(
            "- Directory: pwd=/repo/warp, home_dir=/Users/me, file_symbols_indexed=true\n"
        )
    );
    assert!(prompt.contains("Memory: enabled"));
    assert!(prompt.contains("Warp Drive context: enabled"));
}

#[test]
fn empty_pwd_and_shell_fall_back_to_unknown_and_omit_shell_line() {
    let mut request = request_fixture();
    let context = context_of_mut(&mut request);
    context.directory = Some(api::input_context::Directory::default());
    context.shell = Some(api::input_context::Shell::default());

    let prompt = format_system_prompt(&PromptBuildInput::from_request_context(&request, false));

    assert!(prompt.contains("- Working directory: unknown\n"));
    assert!(!prompt.contains("- Shell:"));
    assert!(
        prompt.contains("Directory: pwd=unknown, home_dir=unknown, file_symbols_indexed=false")
    );
}

#[test]
fn shell_without_version_renders_name_only() {
    let shell = api::input_context::Shell {
        name: "bash".to_string(),
        version: String::new(),
    };
    assert_eq!(shell_label(&shell).as_deref(), Some("bash"));
}

#[test]
fn request_settings_toggle_memory_and_drive_context() {
    let mut request = request_fixture();
    request.settings = Some(api::request::Settings::default());

    let prompt = format_system_prompt(&PromptBuildInput::from_request_context(&request, false));

    assert!(prompt.contains("Memory: disabled; do not assume stored user memory or rules"));
    assert!(prompt.contains("Warp Drive context: disabled"));

    request.settings = None;
    let prompt = format_system_prompt(&PromptBuildInput::from_request_context(&request, false));
    assert!(prompt.contains("Memory: disabled"));
}

#[test]
fn plan_permission_mode_renders_plan_label() {
    assert_eq!(
        permission_mode_label(LocalRuntimePermissionMode::Plan),
        "plan"
    );
    assert_eq!(
        permission_mode_label(LocalRuntimePermissionMode::AcceptEdits),
        "accept-edits"
    );
    assert_eq!(
        permission_mode_label(LocalRuntimePermissionMode::Default),
        "default"
    );

    let prompt = format_system_prompt(&PromptBuildInput {
        permission_mode: Some(permission_mode_label(LocalRuntimePermissionMode::Plan).to_string()),
        ..Default::default()
    });
    assert!(prompt.contains("- Permission mode: plan\n"));
}

#[test]
fn current_time_renders_rfc3339() {
    let mut request = request_fixture();
    context_of_mut(&mut request).current_time = Some(prost_types::Timestamp {
        seconds: 1_700_000_000,
        nanos: 0,
    });

    let lines = render_request_context(context_of(&request), false);

    assert!(lines.contains(&"Current time: 2023-11-14T22:13:20+00:00".to_string()));
}

#[test]
fn invalid_current_time_falls_back_to_epoch_seconds() {
    let timestamp = prost_types::Timestamp {
        seconds: 42,
        nanos: -1,
    };
    assert_eq!(timestamp_for_prompt(&timestamp), "42s since epoch");
}

#[test]
fn image_attachment_line_notes_visibility_for_vision_models() {
    let mut request = request_fixture();
    context_of_mut(&mut request).images.push(image_fixture());

    let vision_lines = render_request_context(context_of(&request), true);
    let vision_line = vision_lines
        .iter()
        .find(|line| line.starts_with("Image attachment:"))
        .expect("image line rendered");
    assert!(vision_line.contains("mime_type=image/png"));
    assert!(vision_line.contains("size=128 bytes"));
    assert!(vision_line.contains("you can see its pixels"));

    let text_only_lines = render_request_context(context_of(&request), false);
    let text_only_line = text_only_lines
        .iter()
        .find(|line| line.starts_with("Image attachment:"))
        .expect("image line rendered");
    assert!(text_only_line.contains("this model cannot see image pixels"));
    assert_ne!(vision_line, text_only_line);
}

#[test]
fn context_lines_are_capped_at_max_context_lines() {
    let mut request = request_fixture();
    let extra = 6;
    context_of_mut(&mut request).selected_text = (0..MAX_CONTEXT_LINES + extra)
        .map(|index| api::input_context::SelectedText {
            text: format!("selection {index}"),
        })
        .collect();

    let input = PromptBuildInput::from_request_context(&request, false);
    // The fixture's directory line occupies one of the visible slots.
    assert_eq!(input.context_lines.len(), MAX_CONTEXT_LINES + extra + 1);

    let prompt = format_system_prompt(&input);
    assert_eq!(
        prompt.matches("- Selected text: selection ").count(),
        MAX_CONTEXT_LINES - 1
    );
    assert!(prompt.contains(&format!(
        "- ... {} additional context items omitted\n",
        extra + 1
    )));
}

#[test]
fn git_context_renders_branch_repository_and_pull_request() {
    let mut request = request_fixture();
    context_of_mut(&mut request).git = Some(api::input_context::Git {
        head: "abc123".to_string(),
        branch: "main".to_string(),
        repository: Some(api::input_context::git::Repository {
            name: "warp".to_string(),
            owner: "warpdotdev".to_string(),
            host: String::new(),
        }),
        pull_request: Some(api::input_context::git::PullRequest {
            number: 42,
            state: PullRequestState::OpenDraft as i32,
            base_branch: "master".to_string(),
            url: "https://github.com/warpdotdev/warp/pull/42".to_string(),
        }),
    });

    let lines = render_request_context(context_of(&request), false);

    assert!(lines.contains(&"Git: branch=main, head=abc123".to_string()));
    assert!(lines.contains(&"Repository: name=warp, owner=warpdotdev, host=unknown".to_string()));
    let pull_request_line = "Pull request: number=42, state=OPEN, draft=true, base_branch=master, \
                             url=https://github.com/warpdotdev/warp/pull/42";
    assert!(lines.contains(&pull_request_line.to_string()));
}

#[test]
fn project_rules_and_files_render_paths_and_truncated_content() {
    let mut request = request_fixture();
    let context = context_of_mut(&mut request);
    context
        .project_rules
        .push(api::input_context::ProjectRules {
            root_path: "/repo/warp".to_string(),
            active_rule_files: vec![api::FileContent {
                file_path: "AGENTS.md".to_string(),
                content: "rules".to_string(),
                line_range: None,
            }],
            additional_rule_file_paths: vec![".rules/extra.md".to_string()],
        });
    context.files.push(api::input_context::File {
        content: Some(api::FileContent {
            file_path: "src/main.rs".to_string(),
            content: "x".repeat(700),
            line_range: Some(api::FileContentLineRange { start: 1, end: 20 }),
        }),
    });
    context
        .files
        .push(api::input_context::File { content: None });

    let lines = render_request_context(context_of(&request), false);

    let rules_line = "Project rules: root_path=/repo/warp, active_rules=AGENTS.md, \
                      additional_rule_paths=.rules/extra.md";
    assert!(lines.contains(&rules_line.to_string()));
    let file_line = lines
        .iter()
        .find(|line| line.starts_with("File context:"))
        .expect("file line rendered");
    assert!(file_line.starts_with("File context: file=src/main.rs, line_range=1-20, content="));
    assert!(file_line.ends_with(TRUNCATION_MARKER));
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.starts_with("File context:"))
            .count(),
        1
    );
}

#[test]
fn skills_context_renders_reference_scope_and_provider() {
    let mut request = request_fixture();
    context_of_mut(&mut request).updated_skills_context = Some(api::input_context::SkillsContext {
        available_skills: vec![api::SkillDescriptor {
            name: "deploy".to_string(),
            description: "Ship it".to_string(),
            provider: Some(api::skill_descriptor::Provider {
                r#type: Some(SkillProvider::Claude(())),
            }),
            scope: Some(api::skill_descriptor::Scope {
                r#type: Some(SkillScope::Project(())),
            }),
            skill_reference: Some(SkillReference::Path(
                "/repo/.claude/skills/deploy/SKILL.md".to_string(),
            )),
        }],
    });

    let lines = render_request_context(context_of(&request), false);

    let skills_line = "Skills available in request context: deploy \
                       (reference: /repo/.claude/skills/deploy/SKILL.md, scope: Project, \
                       provider: Claude): Ship it";
    assert!(lines.contains(&skills_line.to_string()));
}

#[test]
fn mcp_counts_prefer_server_grouped_context() {
    let tool = api::request::mcp_context::McpTool::default();
    let resource = api::request::mcp_context::McpResource::default();
    let grouped = api::request::McpContext {
        servers: vec![
            api::request::mcp_context::McpServer {
                tools: vec![tool.clone(), tool.clone()],
                resources: vec![resource.clone()],
                ..Default::default()
            },
            api::request::mcp_context::McpServer {
                tools: vec![tool.clone()],
                resources: vec![resource.clone(), resource.clone(), resource.clone()],
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    assert_eq!(count_mcp_context(&grouped), (2, 3, 4));

    #[allow(deprecated)]
    let flat = api::request::McpContext {
        tools: vec![tool],
        resources: vec![resource.clone(), resource],
        servers: Vec::new(),
    };
    assert_eq!(count_mcp_context(&flat), (0, 1, 2));

    let mut request = request_fixture();
    request.mcp_context = Some(grouped);
    let prompt = format_system_prompt(&PromptBuildInput::from_request_context(&request, false));
    assert!(prompt.contains("MCP context visible: 2 servers, 3 tools, 4 resources"));
    assert!(prompt.contains("MCP execution is not connected to this local runtime"));
}

#[test]
fn request_prompt_includes_dynamic_runtime_context() {
    let prompt = format_system_prompt(&PromptBuildInput {
        working_directory: Some("/repo/warp".to_string()),
        memory_enabled: true,
        warp_drive_context_enabled: true,
        permission_mode: Some("default".to_string()),
        local_tool_names: vec![
            "read_files".to_string(),
            "grep".to_string(),
            "ask_user_question".to_string(),
        ],
        context_lines: vec!["Git: branch=main, head=abc123".to_string()],
        ..Default::default()
    });

    assert!(prompt.contains(SYSTEM_PROMPT));
    assert!(prompt.contains("Working directory: /repo/warp"));
    assert!(prompt.contains("Executable local tools: read_files, grep, ask_user_question"));
    assert!(prompt.contains("Permission mode: default"));
    assert!(prompt.contains("Capabilities without a matching executable schema"));
    assert!(prompt.contains("Git: branch=main, head=abc123"));
}

#[test]
fn qwen_model_prompt_includes_family_pack_addendum() {
    let prompt = format_system_prompt(&PromptBuildInput {
        model_family: ModelFamily::Qwen,
        local_tool_names: vec!["edit_files".to_string()],
        ..Default::default()
    });
    assert!(prompt.contains("## Model Pack: Qwen"));
    assert!(prompt.contains("edit_files"));
}

#[test]
fn generic_model_prompt_omits_family_pack_markers() {
    let prompt = format_system_prompt(&PromptBuildInput {
        model_family: ModelFamily::Generic,
        ..Default::default()
    });
    assert!(!prompt.contains("## Model Pack:"));
}

#[test]
fn vision_capability_line_reflects_model_support() {
    let vision_prompt = format_system_prompt(&PromptBuildInput {
        vision_enabled: true,
        ..Default::default()
    });
    assert!(vision_prompt.contains("Vision (image understanding): enabled"));

    let text_only_prompt = format_system_prompt(&PromptBuildInput {
        vision_enabled: false,
        ..Default::default()
    });
    assert!(text_only_prompt.contains("Vision (image understanding): disabled"));
    assert!(
        text_only_prompt.contains("this model cannot see image pixels, only attachment metadata")
    );
}
