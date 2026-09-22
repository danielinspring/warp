//! Conversions between runtime tool calls and Warp's protobuf tool messages, the tool schemas
//! advertised to the model, and the argument validators shared by both directions.

use std::collections::HashMap;

use local_agent_runtime::tools::schema::{ToolSchema, ToolSchemaBuilder};
use local_agent_runtime::{ToolCall, ToolExecutionError};
use serde_json::Value;
use warp_multi_agent_api as api;

use crate::registry::{LocalRuntimeToolRegistry, LocalRuntimeToolRoute, LocalRuntimeToolRouteKind};

pub const LOCAL_RUN_AGENTS_MAX_CHILDREN: usize = 4;

const BUNDLED_SKILL_PREFIX: &str = "@warp-skill:";

/// A skill the model can read, addressed by its SKILL.md path or by its bundled id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillRef {
    Path(String),
    BundledSkillId(String),
}

impl SkillRef {
    /// Parse the reference form shown to the model (`@warp-skill:<id>` or a path).
    pub fn parse(value: &str) -> Self {
        match value.strip_prefix(BUNDLED_SKILL_PREFIX) {
            Some(id) => Self::BundledSkillId(id.to_string()),
            None => Self::Path(value.to_string()),
        }
    }

    pub fn from_descriptor(reference: &api::skill_descriptor::SkillReference) -> Self {
        match reference {
            api::skill_descriptor::SkillReference::Path(path) => Self::Path(path.clone()),
            api::skill_descriptor::SkillReference::BundledSkillId(id) => {
                Self::BundledSkillId(id.clone())
            }
        }
    }

    /// The reference form shown to the model in `list_skills` and accepted by `read_skill`.
    pub fn reference_string(&self) -> String {
        match self {
            Self::Path(path) => path.clone(),
            Self::BundledSkillId(id) => format!("{BUNDLED_SKILL_PREFIX}{id}"),
        }
    }

    fn to_read_skill_reference(&self) -> api::message::tool_call::read_skill::SkillReference {
        match self {
            Self::Path(path) => {
                api::message::tool_call::read_skill::SkillReference::SkillPath(path.clone())
            }
            Self::BundledSkillId(id) => {
                api::message::tool_call::read_skill::SkillReference::BundledSkillId(id.clone())
            }
        }
    }
}

/// Build the standard set of tool schemas that can be advertised to the LLM.
///
/// These correspond to `api::ToolType` values and match what Warp's backend
/// advertises — but expressed as OpenAI function-calling schemas for local use.
pub fn build_tool_schemas() -> Vec<ToolSchema> {
    vec![
        ToolSchemaBuilder::new(
            "run_shell_command",
            "Run a shell command in the user's terminal. Use this for any system operation. For read-only lookups (find, ls, pwd, cat, mdfind, grep, python3 -c, etc.) set is_read_only=true so the command can auto-run. On macOS prefer python3 (not python). For quick math/scripts use: python3 -c 'print(sum(range(1, 101)))'. To locate a directory by name outside the project, prefer a FAST scoped search — on macOS: mdfind 'kMDItemFSName == \"folder-name\"c' | head -20; or find ~/codish -maxdepth 5 -type d -name 'folder-name' 2>/dev/null. Avoid unbounded find ~ without -maxdepth. For servers/tests that must keep running, set wait_until_complete=false, then use read_shell_command_output / write_to_long_running_shell_command with the returned block_id.",
        )
        .required_string("command", "The shell command to execute")
        .optional_bool("is_read_only", "Set true for commands with no side effects (find, ls, pwd, cat, mdfind, grep, python3 -c). Defaults to an automatic read-only heuristic when omitted.")
        .optional_bool("is_risky", "Whether the command should require user confirmation")
        .optional_bool("uses_pager", "Whether the command may open a pager")
        .optional_bool(
            "wait_until_complete",
            "When true (default), wait for the command to finish (capped). When false, return a long-running snapshot with block_id so you can poll or write with the LRC tools.",
        )
        .build(),
        ToolSchemaBuilder::new(
            "read_shell_command_output",
            "Read output from a long-running shell command previously started with wait_until_complete=false. Use the block_id from the long-running snapshot.",
        )
        .required_string(
            "block_id",
            "Block id from a long-running shell snapshot (also accepted as command_id)",
        )
        .optional_bool(
            "wait_until_complete",
            "When true, wait until the command finishes before returning. When false/omitted, return the current snapshot after a short delay.",
        )
        .build(),
        ToolSchemaBuilder::new(
            "write_to_long_running_shell_command",
            "Write input to a long-running shell command (REPL/server) identified by block_id. Prefer mode=line for interactive shells.",
        )
        .required_string(
            "block_id",
            "Block id from a long-running shell snapshot (also accepted as command_id)",
        )
        .required_string("input", "Text or bytes to write to the PTY")
        .optional_string(
            "mode",
            "Write mode: raw (default), line (send enter after text), or block (bracketed paste)",
        )
        .build(),
        ToolSchemaBuilder::new(
            "read_files",
            "Read the contents of one or more files. Returns file content with line numbers.",
        )
        .required_string_array(
            "paths",
            "File paths to read (absolute or relative to the current working directory)",
        )
        .build(),
        ToolSchemaBuilder::new(
            "grep",
            "Search file contents using a regex pattern. Returns matching lines with file paths.",
        )
        .required_string_array("queries", "Regex patterns to search for")
        .optional_string("path", "Directory to search in (defaults to cwd)")
        .build(),
        ToolSchemaBuilder::new(
            "file_glob_v2",
            "Find files matching glob patterns under search_dir (defaults to the current project/working directory). This is project-scoped and lists files, not an arbitrary filesystem folder search. To find a directory by name under the home folder, use run_shell_command with is_read_only=true (prefer mdfind or find with -maxdepth under a known path like ~/codish), not file_glob_v2.",
        )
        .required_string_array(
            "patterns",
            "Glob patterns such as '**/*.rs' or 'src/**/*.ts'",
        )
        .optional_string(
            "search_dir",
            "Base directory to search from (defaults to the current working directory)",
        )
        .build(),
        ToolSchemaBuilder::new(
            "search_codebase",
            "Semantic search across the codebase. Use for finding concepts, functions, or implementations.",
        )
        .required_string("query", "Natural language search query")
        .optional_string_array("path_filters", "Optional path prefixes to restrict the search")
        .optional_string("codebase_path", "Optional codebase root path")
        .build(),
        ToolSchema {
            name: "edit_files".to_string(),
            description: "Propose reviewed file edits using Warp's CodeDiff UI (never writes directly to disk). REQUIRED for any file modifications. Call this instead of using shell to write files. Supports 'replace' (with search/replace), 'create' (with content), and 'delete'. Always provide precise 'edits' array.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "title": {
                        "type": "string",
                        "description": "Optional short title for the edit set"
                    },
                    "edits": {
                        "type": "array",
                        "description": "List of edits to perform. Each item must have 'type' and 'file'. For 'replace' also provide exact 'search' string to find and 'replace' string. For 'create' provide 'content'. Matches must be exact for replace.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "type": {
                                    "type": "string",
                                    "enum": ["replace", "create", "delete"],
                                    "description": "The kind of edit"
                                },
                                "file": {
                                    "type": "string",
                                    "description": "Absolute or relative path to the target file"
                                },
                                "search": {
                                    "type": "string",
                                    "description": "The exact existing text to find and replace (for type=replace). Must match precisely."
                                },
                                "replace": {
                                    "type": "string",
                                    "description": "The new text to insert in place of search (for type=replace)"
                                },
                                "content": {
                                    "type": "string",
                                    "description": "The full new file content (for type=create)"
                                }
                            },
                            "required": ["type", "file"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["edits"],
                "additionalProperties": false
            }),
        },
    ]
}

pub(crate) fn ask_user_question_schema() -> ToolSchema {
    ToolSchema {
        name: "ask_user_question".to_string(),
        description: "Pause and ask the user one or more multiple-choice clarification questions."
            .to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": {
                            "question_id": { "type": "string" },
                            "question": { "type": "string" },
                            "options": {
                                "type": "array",
                                "minItems": 2,
                                "items": { "type": "string" }
                            },
                            "recommended_option_index": { "type": "integer", "minimum": -1 },
                            "is_multiselect": { "type": "boolean" },
                            "supports_other": { "type": "boolean" }
                        },
                        "required": ["question_id", "question", "options"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["questions"],
            "additionalProperties": false
        }),
    }
}

pub(crate) fn run_agents_schema() -> ToolSchema {
    ToolSchema {
        name: "run_agents".to_string(),
        description: format!(
            "Delegate independent work to up to {LOCAL_RUN_AGENTS_MAX_CHILDREN} parallel child agents via Warp orchestration. Use only for parallelizable subtasks; prefer direct tools for sequential work. Local execution only. Child agents cannot call run_agents again."
        ),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "summary": {
                    "type": "string",
                    "description": "Short human-readable summary of what the child agents will do"
                },
                "base_prompt": {
                    "type": "string",
                    "description": "Optional shared instructions prepended to every child agent prompt"
                },
                "agents": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": LOCAL_RUN_AGENTS_MAX_CHILDREN,
                    "description": "One entry per child agent to launch",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": {
                                "type": "string",
                                "description": "Stable short name for the child agent"
                            },
                            "prompt": {
                                "type": "string",
                                "description": "Task prompt for this child agent"
                            },
                            "title": {
                                "type": "string",
                                "description": "Optional display title for the child conversation"
                            }
                        },
                        "required": ["name", "prompt"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["summary", "agents"],
            "additionalProperties": false
        }),
    }
}

pub(crate) fn read_documents_schema() -> ToolSchema {
    ToolSchemaBuilder::new(
        "read_documents",
        "Read Warp AI documents by UUID. Use document IDs from conversation context or prior create/edit results.",
    )
    .required_string_array(
        "document_ids",
        "One or more AI document UUIDs to read",
    )
    .build()
}

pub(crate) fn edit_documents_schema() -> ToolSchema {
    ToolSchema {
        name: "edit_documents".to_string(),
        description: "Apply search/replace edits to existing Warp AI documents by UUID."
            .to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "diffs": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": {
                            "document_id": { "type": "string", "description": "AI document UUID" },
                            "search": { "type": "string" },
                            "replace": { "type": "string" }
                        },
                        "required": ["document_id", "search", "replace"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["diffs"],
            "additionalProperties": false
        }),
    }
}

pub(crate) fn create_documents_schema() -> ToolSchema {
    ToolSchema {
        name: "create_documents".to_string(),
        description: "Create one or more new Warp AI documents with title and content.".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "documents": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": {
                            "title": { "type": "string" },
                            "content": { "type": "string" }
                        },
                        "required": ["title", "content"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["documents"],
            "additionalProperties": false
        }),
    }
}

pub(crate) fn request_computer_use_schema() -> ToolSchema {
    ToolSchemaBuilder::new(
        "request_computer_use",
        "Request user approval to control the computer (mouse/keyboard/screenshots). Call before use_computer when permission is required.",
    )
    .required_string(
        "task_summary",
        "Short summary of the computer-use task for the user approval UI",
    )
    .build()
}

pub(crate) fn use_computer_schema() -> ToolSchema {
    ToolSchema {
        name: "use_computer".to_string(),
        description: "Perform local computer-use actions (type text, move/click mouse, wait, keys). Prefer after request_computer_use is approved. Action objects follow Warp computer_use Action JSON (type_text, wait, mouse_move, mouse_down, mouse_up, etc.).".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "action_summary": {
                    "type": "string",
                    "description": "Short summary of this action batch"
                },
                "actions": {
                    "type": "array",
                    "minItems": 1,
                    "description": "Ordered computer_use::Action values as JSON objects",
                    "items": { "type": "object" }
                },
                "take_screenshot": {
                    "type": "boolean",
                    "description": "When true, capture a screenshot after actions (metadata returned; image not embedded for local models)"
                }
            },
            "required": ["action_summary", "actions"],
            "additionalProperties": false
        }),
    }
}

pub fn tool_call_to_proto_tool(
    call: &ToolCall,
) -> Result<api::message::tool_call::Tool, ToolExecutionError> {
    use api::message::tool_call::Tool;

    match call.name.as_str() {
        "run_shell_command" => {
            validate_allowed_arguments(
                &call.arguments,
                &[
                    "command",
                    "is_read_only",
                    "is_risky",
                    "uses_pager",
                    "wait_until_complete",
                ],
                &call.name,
            )?;
            let command = required_string(&call.arguments, "command", &call.name)?;
            let explicit_read_only = optional_bool(&call.arguments, "is_read_only")?;
            let is_read_only =
                explicit_read_only.unwrap_or_else(|| infer_shell_command_is_read_only(&command));
            // Default true for weak local models; false enables LRC poll/write tools.
            let wait_until_complete =
                optional_bool(&call.arguments, "wait_until_complete")?.unwrap_or(true);
            Ok(Tool::RunShellCommand(
                api::message::tool_call::RunShellCommand {
                    command,
                    is_read_only,
                    is_risky: optional_bool(&call.arguments, "is_risky")?.unwrap_or(false),
                    uses_pager: optional_bool(&call.arguments, "uses_pager")?.unwrap_or(false),
                    wait_until_complete_value: Some(
                        api::message::tool_call::run_shell_command::WaitUntilCompleteValue::WaitUntilComplete(
                            wait_until_complete,
                        ),
                    ),
                    ..Default::default()
                },
            ))
        }
        "read_shell_command_output" => Ok(Tool::ReadShellCommandOutput(
            read_shell_command_output_tool_call_to_proto(call)?,
        )),
        "write_to_long_running_shell_command" => Ok(Tool::WriteToLongRunningShellCommand(
            write_to_long_running_shell_command_tool_call_to_proto(call)?,
        )),
        "read_files" => {
            validate_allowed_arguments(&call.arguments, &["paths", "files"], &call.name)?;
            Ok(Tool::ReadFiles(api::message::tool_call::ReadFiles {
                files: required_string_array(&call.arguments, &["paths", "files"], &call.name)?
                    .into_iter()
                    .map(|path| api::message::tool_call::read_files::File {
                        name: path,
                        line_ranges: vec![],
                    })
                    .collect(),
            }))
        }
        "grep" => {
            validate_allowed_arguments(
                &call.arguments,
                &["queries", "patterns", "pattern", "path"],
                &call.name,
            )?;
            Ok(Tool::Grep(api::message::tool_call::Grep {
                queries: if has_any_argument(&call.arguments, &["queries", "patterns"]) {
                    required_string_array(&call.arguments, &["queries", "patterns"], &call.name)?
                } else {
                    vec![required_string(&call.arguments, "pattern", &call.name)?]
                },
                path: optional_string(&call.arguments, "path")?.unwrap_or_else(|| ".".to_string()),
            }))
        }
        "file_glob_v2" => {
            validate_allowed_arguments(
                &call.arguments,
                &["patterns", "pattern", "search_dir", "path"],
                &call.name,
            )?;
            let search_dir = match optional_string(&call.arguments, "search_dir")? {
                Some(search_dir) => search_dir,
                None => optional_string(&call.arguments, "path")?.unwrap_or_default(),
            };
            Ok(Tool::FileGlobV2(api::message::tool_call::FileGlobV2 {
                patterns: if has_any_argument(&call.arguments, &["patterns"]) {
                    required_string_array(&call.arguments, &["patterns"], &call.name)?
                } else {
                    vec![required_string(&call.arguments, "pattern", &call.name)?]
                },
                search_dir,
                min_depth: 0,
                max_depth: 0,
                max_matches: 0,
            }))
        }
        "search_codebase" => {
            validate_allowed_arguments(
                &call.arguments,
                &["query", "path_filters", "codebase_path"],
                &call.name,
            )?;
            Ok(Tool::SearchCodebase(
                api::message::tool_call::SearchCodebase {
                    query: required_string(&call.arguments, "query", &call.name)?,
                    path_filters: optional_string_array(&call.arguments, "path_filters")?
                        .unwrap_or_default(),
                    codebase_path: optional_string(&call.arguments, "codebase_path")?
                        .unwrap_or_default(),
                },
            ))
        }
        "edit_files" => Ok(Tool::ApplyFileDiffs(edit_files_tool_call_to_proto(call)?)),
        "ask_user_question" => Ok(Tool::AskUserQuestion(ask_user_question_tool_call_to_proto(
            call,
        )?)),
        "run_agents" => Ok(Tool::RunAgents(run_agents_tool_call_to_proto(call)?)),
        _ => Err(ToolExecutionError::NotFound {
            name: call.name.clone(),
        }),
    }
}

fn read_shell_command_output_tool_call_to_proto(
    call: &ToolCall,
) -> Result<api::message::tool_call::ReadShellCommandOutput, ToolExecutionError> {
    validate_allowed_arguments(
        &call.arguments,
        &["block_id", "command_id", "wait_until_complete"],
        &call.name,
    )?;
    let command_id = required_string_any(&call.arguments, &["block_id", "command_id"], &call.name)?;
    let wait_until_complete =
        optional_bool(&call.arguments, "wait_until_complete")?.unwrap_or(false);
    Ok(api::message::tool_call::ReadShellCommandOutput {
        command_id,
        delay: wait_until_complete
            .then_some(api::message::tool_call::read_shell_command_output::Delay::OnCompletion(())),
    })
}

fn write_to_long_running_shell_command_tool_call_to_proto(
    call: &ToolCall,
) -> Result<api::message::tool_call::WriteToLongRunningShellCommand, ToolExecutionError> {
    use api::message::tool_call::write_to_long_running_shell_command::Mode;
    use api::message::tool_call::write_to_long_running_shell_command::mode::Mode as ModeVariant;

    validate_allowed_arguments(
        &call.arguments,
        &["block_id", "command_id", "input", "mode"],
        &call.name,
    )?;
    let command_id = required_string_any(&call.arguments, &["block_id", "command_id"], &call.name)?;
    let input = required_string(&call.arguments, "input", &call.name)?;
    let mode = match optional_string(&call.arguments, "mode")?
        .unwrap_or_else(|| "raw".to_string())
        .to_ascii_lowercase()
        .as_str()
    {
        "raw" => ModeVariant::Raw(()),
        "line" => ModeVariant::Line(()),
        "block" => ModeVariant::Block(()),
        other => {
            return Err(ToolExecutionError::InvalidInput {
                reason: format!(
                    "Tool `write_to_long_running_shell_command` mode must be raw, line, or block (got {other})"
                ),
            });
        }
    };
    Ok(api::message::tool_call::WriteToLongRunningShellCommand {
        input: input.into_bytes(),
        mode: Some(Mode { mode: Some(mode) }),
        command_id,
    })
}

fn run_agents_tool_call_to_proto(call: &ToolCall) -> Result<api::RunAgents, ToolExecutionError> {
    validate_allowed_arguments(
        &call.arguments,
        &["summary", "base_prompt", "agents"],
        &call.name,
    )?;
    let summary = required_string(&call.arguments, "summary", &call.name)?;
    let base_prompt = optional_string(&call.arguments, "base_prompt")?.unwrap_or_default();
    let agents = call
        .arguments
        .get("agents")
        .and_then(Value::as_array)
        .ok_or_else(|| ToolExecutionError::InvalidInput {
            reason: "Tool `run_agents` requires array argument `agents`".to_string(),
        })?;
    if agents.is_empty() {
        return Err(ToolExecutionError::InvalidInput {
            reason: "Tool `run_agents` requires at least one agent".to_string(),
        });
    }
    if agents.len() > LOCAL_RUN_AGENTS_MAX_CHILDREN {
        return Err(ToolExecutionError::InvalidInput {
            reason: format!(
                "Tool `run_agents` allows at most {LOCAL_RUN_AGENTS_MAX_CHILDREN} child agents"
            ),
        });
    }

    Ok(api::RunAgents {
        summary,
        base_prompt,
        skills: Vec::new(),
        // Leave model/harness empty so Warp's orchestration UI / profile defaults apply.
        model_id: String::new(),
        harness: None,
        agent_run_configs: agents
            .iter()
            .map(parse_run_agents_agent)
            .collect::<Result<Vec<_>, _>>()?,
        plan_id: String::new(),
        // Local-runtime orchestration is forced to local execution (no remote fan-out).
        execution_mode: Some(api::run_agents::ExecutionModeOneOf::Local(
            api::run_agents::Local {},
        )),
    })
}

fn parse_run_agents_agent(
    value: &Value,
) -> Result<api::run_agents::AgentRunConfig, ToolExecutionError> {
    let arguments = Value::Object(
        value
            .as_object()
            .ok_or_else(|| ToolExecutionError::InvalidInput {
                reason: "`run_agents.agents` entries must be objects".to_string(),
            })?
            .clone(),
    );
    validate_allowed_arguments(&arguments, &["name", "prompt", "title"], "run_agents agent")?;
    Ok(api::run_agents::AgentRunConfig {
        name: required_string(&arguments, "name", "run_agents agent")?,
        prompt: required_string(&arguments, "prompt", "run_agents agent")?,
        title: optional_string(&arguments, "title")?.unwrap_or_default(),
        agent_identity_uid: String::new(),
        model_id: String::new(),
        harness: None,
        execution_mode: None,
    })
}

fn ask_user_question_tool_call_to_proto(
    call: &ToolCall,
) -> Result<api::AskUserQuestion, ToolExecutionError> {
    validate_allowed_arguments(&call.arguments, &["questions"], &call.name)?;
    let questions = call
        .arguments
        .get("questions")
        .and_then(Value::as_array)
        .ok_or_else(|| ToolExecutionError::InvalidInput {
            reason: "Tool `ask_user_question` requires array argument `questions`".to_string(),
        })?;
    if questions.is_empty() {
        return Err(ToolExecutionError::InvalidInput {
            reason: "Tool `ask_user_question` requires at least one question".to_string(),
        });
    }

    Ok(api::AskUserQuestion {
        questions: questions
            .iter()
            .map(parse_ask_user_question)
            .collect::<Result<_, _>>()?,
    })
}

fn parse_ask_user_question(
    value: &Value,
) -> Result<api::ask_user_question::Question, ToolExecutionError> {
    use api::ask_user_question::question::QuestionType;

    let arguments = Value::Object(
        value
            .as_object()
            .ok_or_else(|| ToolExecutionError::InvalidInput {
                reason: "`ask_user_question.questions` entries must be objects".to_string(),
            })?
            .clone(),
    );
    validate_allowed_arguments(
        &arguments,
        &[
            "question_id",
            "question",
            "options",
            "recommended_option_index",
            "is_multiselect",
            "supports_other",
        ],
        "ask_user_question question",
    )?;
    let labels = required_string_array(&arguments, &["options"], "ask_user_question question")?;
    if labels.len() < 2 {
        return Err(ToolExecutionError::InvalidInput {
            reason: "`ask_user_question` requires at least two options per question".to_string(),
        });
    }
    let recommended_option_index = optional_i64(&arguments, "recommended_option_index")?
        .and_then(|index| usize::try_from(index).ok())
        .filter(|index| *index < labels.len())
        .and_then(|index| i32::try_from(index).ok())
        .unwrap_or(-1);

    Ok(api::ask_user_question::Question {
        question_id: required_string(&arguments, "question_id", "ask_user_question question")?,
        question: required_string(&arguments, "question", "ask_user_question question")?,
        question_type: Some(QuestionType::MultipleChoice(
            api::ask_user_question::MultipleChoice {
                options: labels
                    .into_iter()
                    .map(|label| api::ask_user_question::Option { label })
                    .collect(),
                recommended_option_index,
                is_multiselect: optional_bool(&arguments, "is_multiselect")?.unwrap_or(false),
                supports_other: optional_bool(&arguments, "supports_other")?.unwrap_or(true),
            },
        )),
    })
}

pub fn tool_call_to_proto_tool_with_registry(
    call: &ToolCall,
    registry: &LocalRuntimeToolRegistry,
) -> Result<api::message::tool_call::Tool, ToolExecutionError> {
    use api::message::tool_call::Tool;

    let route = registry
        .route(&call.name)
        .ok_or_else(|| ToolExecutionError::NotFound {
            name: call.name.clone(),
        })?;
    match &route.kind {
        LocalRuntimeToolRouteKind::BuiltIn => tool_call_to_proto_tool(call),
        LocalRuntimeToolRouteKind::ListSkills
        | LocalRuntimeToolRouteKind::LocalWeb
        | LocalRuntimeToolRouteKind::LocalTodo
        | LocalRuntimeToolRouteKind::LocalGit => Err(ToolExecutionError::InvalidInput {
            reason: format!(
                "{} has no wire proto tool form; results stay in the runtime transcript envelope",
                call.name
            ),
        }),
        LocalRuntimeToolRouteKind::McpTool { server_id, name } => {
            let arguments = arguments_object(&call.arguments, &call.name)?;
            Ok(Tool::CallMcpTool(api::message::tool_call::CallMcpTool {
                server_id: server_id.map(|id| id.to_string()).unwrap_or_default(),
                name: name.clone(),
                args: Some(json_object_to_prost_struct(arguments)?),
            }))
        }
        LocalRuntimeToolRouteKind::ReadMcpResource => {
            validate_allowed_arguments(&call.arguments, &["name", "uri"], &call.name)?;
            let uri = optional_string(&call.arguments, "uri")?
                .or(optional_string(&call.arguments, "name")?)
                .ok_or_else(|| ToolExecutionError::InvalidInput {
                    reason: "Tool `read_mcp_resource` requires `uri` or `name`".to_string(),
                })?;
            Ok(Tool::ReadMcpResource(
                api::message::tool_call::ReadMcpResource {
                    server_id: String::new(),
                    uri,
                },
            ))
        }
        LocalRuntimeToolRouteKind::ReadSkill { skill_lookup } => {
            validate_allowed_arguments(&call.arguments, &["skill"], &call.name)?;
            let skill = required_string(&call.arguments, "skill", &call.name)?;
            let reference = skill_lookup
                .get(&skill)
                .cloned()
                .unwrap_or_else(|| SkillRef::parse(&skill));
            Ok(Tool::ReadSkill(api::message::tool_call::ReadSkill {
                name: skill,
                skill_reference: Some(reference.to_read_skill_reference()),
            }))
        }
    }
}

pub fn proto_tool_call_to_runtime_with_registry(
    tool_call: &api::message::ToolCall,
    registry: &LocalRuntimeToolRegistry,
) -> Option<ToolCall> {
    use api::message::tool_call::Tool;
    use api::message::tool_call::read_skill::SkillReference as ProtoSkillReference;

    let tool = tool_call.tool.as_ref()?;
    let (name, arguments) = match tool {
        Tool::RunShellCommand(tool) => {
            let wait_until_complete = tool.wait_until_complete_value.is_none_or(
                |api::message::tool_call::run_shell_command::WaitUntilCompleteValue::WaitUntilComplete(
                    should_wait,
                )| should_wait,
            );
            (
                "run_shell_command".to_string(),
                serde_json::json!({
                    "command": tool.command,
                    "is_read_only": tool.is_read_only,
                    "is_risky": tool.is_risky,
                    "uses_pager": tool.uses_pager,
                    "wait_until_complete": wait_until_complete,
                }),
            )
        }
        Tool::ReadShellCommandOutput(tool) => {
            let wait_until_complete = matches!(
                tool.delay,
                Some(api::message::tool_call::read_shell_command_output::Delay::OnCompletion(_))
            );
            (
                "read_shell_command_output".to_string(),
                serde_json::json!({
                    "block_id": tool.command_id,
                    "wait_until_complete": wait_until_complete,
                }),
            )
        }
        Tool::WriteToLongRunningShellCommand(tool) => {
            use api::message::tool_call::write_to_long_running_shell_command::mode::Mode as ModeVariant;

            let mode = match tool.mode.as_ref().and_then(|mode| mode.mode.as_ref()) {
                Some(ModeVariant::Line(_)) => "line",
                Some(ModeVariant::Block(_)) => "block",
                Some(ModeVariant::Raw(_)) | None => "raw",
            };
            (
                "write_to_long_running_shell_command".to_string(),
                serde_json::json!({
                    "block_id": tool.command_id,
                    "input": String::from_utf8_lossy(&tool.input),
                    "mode": mode,
                }),
            )
        }
        Tool::ReadFiles(tool) => (
            "read_files".to_string(),
            serde_json::json!({
                "paths": tool.files.iter().map(|file| file.name.clone()).collect::<Vec<_>>(),
            }),
        ),
        Tool::Grep(tool) => (
            "grep".to_string(),
            serde_json::json!({
                "queries": tool.queries,
                "path": tool.path,
            }),
        ),
        Tool::FileGlobV2(tool) => (
            "file_glob_v2".to_string(),
            serde_json::json!({
                "patterns": tool.patterns,
                "search_dir": tool.search_dir,
            }),
        ),
        Tool::SearchCodebase(tool) => (
            "search_codebase".to_string(),
            serde_json::json!({
                "query": tool.query,
                "path_filters": tool.path_filters,
                "codebase_path": tool.codebase_path,
            }),
        ),
        Tool::ApplyFileDiffs(tool) => {
            let mut edits = Vec::new();
            edits.extend(tool.diffs.iter().map(|diff| {
                serde_json::json!({
                    "type": "replace",
                    "file": diff.file_path,
                    "search": diff.search,
                    "replace": diff.replace,
                })
            }));
            edits.extend(tool.new_files.iter().map(|file| {
                serde_json::json!({
                    "type": "create",
                    "file": file.file_path,
                    "content": file.content,
                })
            }));
            edits.extend(tool.deleted_files.iter().map(|file| {
                serde_json::json!({
                    "type": "delete",
                    "file": file.file_path,
                })
            }));
            (
                "edit_files".to_string(),
                serde_json::json!({
                    "title": tool.summary,
                    "edits": edits,
                }),
            )
        }
        Tool::CallMcpTool(tool) => {
            let name = registry
                .mcp_function_name(&tool.server_id, &tool.name)
                .unwrap_or_else(|| {
                    format!(
                        "mcp__restored__{}",
                        sanitize_function_name(tool.name.as_str())
                    )
                });
            (
                name,
                tool.args
                    .as_ref()
                    .map(prost_struct_to_json)
                    .unwrap_or_else(|| serde_json::json!({})),
            )
        }
        Tool::ReadMcpResource(tool) => (
            "read_mcp_resource".to_string(),
            serde_json::json!({ "uri": tool.uri }),
        ),
        Tool::ReadSkill(tool) => {
            let skill = match tool.skill_reference.as_ref()? {
                ProtoSkillReference::SkillPath(path) => path.clone(),
                ProtoSkillReference::BundledSkillId(id) => format!("{BUNDLED_SKILL_PREFIX}{id}"),
            };
            (
                "read_skill".to_string(),
                serde_json::json!({ "skill": skill }),
            )
        }
        Tool::AskUserQuestion(tool) => {
            use api::ask_user_question::question::QuestionType;

            let questions = tool
                .questions
                .iter()
                .filter_map(|question| {
                    let QuestionType::MultipleChoice(multiple_choice) =
                        question.question_type.as_ref()?;
                    Some(serde_json::json!({
                        "question_id": question.question_id,
                        "question": question.question,
                        "options": multiple_choice
                            .options
                            .iter()
                            .map(|option| option.label.clone())
                            .collect::<Vec<_>>(),
                        "recommended_option_index": multiple_choice.recommended_option_index,
                        "is_multiselect": multiple_choice.is_multiselect,
                        "supports_other": multiple_choice.supports_other,
                    }))
                })
                .collect::<Vec<_>>();
            (
                "ask_user_question".to_string(),
                serde_json::json!({ "questions": questions }),
            )
        }
        Tool::RunAgents(tool) => {
            let agents = tool
                .agent_run_configs
                .iter()
                .map(|config| {
                    let mut agent = serde_json::json!({
                        "name": config.name,
                        "prompt": config.prompt,
                    });
                    if !config.title.is_empty() {
                        agent["title"] = Value::String(config.title.clone());
                    }
                    agent
                })
                .collect::<Vec<_>>();
            let mut arguments = serde_json::json!({
                "summary": tool.summary,
                "agents": agents,
            });
            if !tool.base_prompt.is_empty() {
                arguments["base_prompt"] = Value::String(tool.base_prompt.clone());
            }
            ("run_agents".to_string(), arguments)
        }
        _ => return None,
    };

    Some(ToolCall {
        id: tool_call.tool_call_id.clone(),
        name,
        arguments,
    })
}

fn json_object_to_prost_struct(
    object: &serde_json::Map<String, Value>,
) -> Result<prost_types::Struct, ToolExecutionError> {
    object
        .iter()
        .map(|(key, value)| Ok((key.clone(), json_to_prost_value(value)?)))
        .collect::<Result<_, _>>()
        .map(|fields| prost_types::Struct { fields })
}

fn json_to_prost_value(value: &Value) -> Result<prost_types::Value, ToolExecutionError> {
    use prost_types::value::Kind;

    let kind =
        match value {
            Value::Null => Kind::NullValue(0),
            Value::Bool(value) => Kind::BoolValue(*value),
            Value::Number(value) => Kind::NumberValue(value.as_f64().ok_or_else(|| {
                ToolExecutionError::InvalidInput {
                    reason: "MCP numeric argument cannot be represented as f64".to_string(),
                }
            })?),
            Value::String(value) => Kind::StringValue(value.clone()),
            Value::Array(values) => Kind::ListValue(prost_types::ListValue {
                values: values
                    .iter()
                    .map(json_to_prost_value)
                    .collect::<Result<_, _>>()?,
            }),
            Value::Object(object) => Kind::StructValue(json_object_to_prost_struct(object)?),
        };
    Ok(prost_types::Value { kind: Some(kind) })
}

pub(crate) fn prost_struct_to_json(value: &prost_types::Struct) -> Value {
    Value::Object(
        value
            .fields
            .iter()
            .map(|(key, value)| (key.clone(), prost_value_to_json(value)))
            .collect(),
    )
}

fn prost_value_to_json(value: &prost_types::Value) -> Value {
    use prost_types::value::Kind;

    match value.kind.as_ref() {
        Some(Kind::NullValue(_)) | None => Value::Null,
        Some(Kind::BoolValue(value)) => Value::Bool(*value),
        Some(Kind::NumberValue(value)) => serde_json::Number::from_f64(*value)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        Some(Kind::StringValue(value)) => Value::String(value.clone()),
        Some(Kind::ListValue(value)) => {
            Value::Array(value.values.iter().map(prost_value_to_json).collect())
        }
        Some(Kind::StructValue(value)) => prost_struct_to_json(value),
    }
}

fn edit_files_tool_call_to_proto(
    call: &ToolCall,
) -> Result<api::message::tool_call::ApplyFileDiffs, ToolExecutionError> {
    validate_allowed_arguments(&call.arguments, &["title", "edits"], &call.name)?;
    let edits_value = call
        .arguments
        .get("edits")
        .and_then(Value::as_array)
        .ok_or_else(|| ToolExecutionError::InvalidInput {
            reason: "Tool `edit_files` requires array argument `edits`".to_string(),
        })?;

    if edits_value.is_empty() {
        return Err(ToolExecutionError::InvalidInput {
            reason: "Tool `edit_files` requires at least one edit".to_string(),
        });
    }

    let mut diffs = Vec::new();
    let mut new_files = Vec::new();
    let mut deleted_files = Vec::new();

    for edit in edits_value {
        let object = edit
            .as_object()
            .ok_or_else(|| ToolExecutionError::InvalidInput {
                reason: "`edit_files.edits` entries must be objects".to_string(),
            })?;
        let arguments = Value::Object(object.clone());
        validate_allowed_arguments(
            &arguments,
            &["type", "file", "search", "replace", "content"],
            "edit_files edit",
        )?;

        let edit_type = required_string(&arguments, "type", "edit_files edit")?;
        let file_path = required_string(&arguments, "file", "edit_files edit")?;
        match edit_type.as_str() {
            "replace" => {
                diffs.push(api::message::tool_call::apply_file_diffs::FileDiff {
                    file_path,
                    search: required_string(&arguments, "search", "edit_files replace edit")?,
                    replace: required_string(&arguments, "replace", "edit_files replace edit")?,
                });
            }
            "create" => {
                new_files.push(api::message::tool_call::apply_file_diffs::NewFile {
                    file_path,
                    content: required_string(&arguments, "content", "edit_files create edit")?,
                    allow_overwrite: false,
                });
            }
            "delete" => {
                deleted_files
                    .push(api::message::tool_call::apply_file_diffs::DeleteFile { file_path });
            }
            _ => {
                return Err(ToolExecutionError::InvalidInput {
                    reason: "Tool `edit_files` edit type must be `replace`, `create`, or `delete`"
                        .to_string(),
                });
            }
        }
    }

    Ok(api::message::tool_call::ApplyFileDiffs {
        summary: optional_string(&call.arguments, "title")?.unwrap_or_default(),
        diffs,
        new_files,
        deleted_files,
        v4a_updates: vec![],
    })
}

fn has_any_argument(arguments: &Value, names: &[&str]) -> bool {
    names.iter().any(|name| arguments.get(name).is_some())
}

/// Whether a `run_shell_command` call should be treated as read-only for scheduling
/// and Warp auto-execute. Prefers an explicit `is_read_only` argument, otherwise
/// applies a conservative command heuristic.
pub(crate) fn shell_command_is_read_only(arguments: &Value) -> bool {
    if let Some(Value::Bool(is_read_only)) = arguments.get("is_read_only") {
        return *is_read_only;
    }
    arguments
        .get("command")
        .and_then(Value::as_str)
        .is_some_and(infer_shell_command_is_read_only)
}

fn infer_shell_command_is_read_only(command: &str) -> bool {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return false;
    }

    let lowered = trimmed.to_ascii_lowercase();
    const UNSAFE_MARKERS: &[&str] = &[
        " rm ",
        "rm ",
        "sudo ",
        " chmod ",
        " chown ",
        " mv ",
        " cp ",
        " dd ",
        " mkfs",
        " shutdown",
        " reboot",
        " kill ",
        " pkill ",
        " curl ",
        " wget ",
        "| sh",
        "|bash",
        "tee ",
        "sed -i",
        "truncate ",
        "git commit",
        "git push",
        "git reset",
        "git checkout",
        "git switch",
        "npm install",
        "pip install",
        "cargo install",
    ];
    let padded = format!(" {lowered} ");
    if UNSAFE_MARKERS.iter().any(|marker| padded.contains(marker)) {
        return false;
    }
    // Allow discarding stdout/stderr to /dev/null; treat other redirects as writes.
    let without_null_redirects = lowered
        .replace("2>/dev/null", "")
        .replace("2> /dev/null", "")
        .replace(">/dev/null", "")
        .replace("> /dev/null", "");
    if without_null_redirects.contains('>') {
        return false;
    }

    // Strip common wrappers like `cd ... && find ...` and inspect each segment.
    let segments = lowered
        .split(['&', ';', '|', '\n'])
        .map(str::trim)
        .filter(|segment| !segment.is_empty());
    for segment in segments {
        let first = segment
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .trim_start_matches("./");
        const READ_ONLY_COMMANDS: &[&str] = &[
            "ls", "find", "pwd", "cat", "head", "tail", "wc", "which", "type", "echo", "printf",
            "rg", "grep", "egrep", "fgrep", "fd", "tree", "stat", "file", "du", "df", "date",
            "whoami", "id", "env", "printenv", "uname", "hostname", "basename", "dirname",
            "realpath", "readlink", "git", "cd", "true", "false", "test", "[", "mdfind", "locate",
            // Interpreter one-liners are handled below (require -c/-e).
            "python", "python3", "node", "nodejs", "ruby", "perl",
        ];
        if !READ_ONLY_COMMANDS.contains(&first) {
            return false;
        }
        if matches!(
            first,
            "python" | "python3" | "node" | "nodejs" | "ruby" | "perl"
        ) {
            // Only treat pure -c/-e eval one-liners as read-only compute.
            let has_eval_flag = segment.split_whitespace().any(|t| t == "-c" || t == "-e");
            if !has_eval_flag {
                return false;
            }
            continue;
        }
        if first == "git" {
            let sub = segment.split_whitespace().nth(1).unwrap_or_default();
            const READ_ONLY_GIT: &[&str] = &[
                "status",
                "log",
                "diff",
                "show",
                "branch",
                "tag",
                "remote",
                "ls-files",
                "rev-parse",
                "describe",
                "blame",
                "grep",
                "shortlog",
            ];
            if !READ_ONLY_GIT.contains(&sub) {
                return false;
            }
        }
    }
    true
}

fn validate_allowed_arguments(
    arguments: &Value,
    allowed: &[&str],
    tool_name: &str,
) -> Result<(), ToolExecutionError> {
    let object = arguments_object(arguments, tool_name)?;
    let mut unsupported = object
        .keys()
        .filter(|name| !allowed.contains(&name.as_str()))
        .cloned()
        .collect::<Vec<_>>();

    if unsupported.is_empty() {
        return Ok(());
    }

    unsupported.sort();
    Err(ToolExecutionError::InvalidInput {
        reason: format!(
            "Tool `{tool_name}` does not support argument(s): `{}`",
            unsupported.join("`, `")
        ),
    })
}

fn arguments_object<'a>(
    arguments: &'a Value,
    tool_name: &str,
) -> Result<&'a serde_json::Map<String, Value>, ToolExecutionError> {
    arguments
        .as_object()
        .ok_or_else(|| ToolExecutionError::InvalidInput {
            reason: format!("Tool `{tool_name}` requires object arguments"),
        })
}

fn required_string(
    arguments: &Value,
    name: &str,
    tool_name: &str,
) -> Result<String, ToolExecutionError> {
    let value =
        optional_string(arguments, name)?.ok_or_else(|| ToolExecutionError::InvalidInput {
            reason: format!("Tool `{tool_name}` requires non-empty string argument `{name}`"),
        })?;

    if value.trim().is_empty() {
        return Err(ToolExecutionError::InvalidInput {
            reason: format!("Tool `{tool_name}` requires non-empty string argument `{name}`"),
        });
    }

    Ok(value)
}

fn required_string_any(
    arguments: &Value,
    keys: &[&str],
    tool_name: &str,
) -> Result<String, ToolExecutionError> {
    for key in keys {
        if let Some(value) = optional_string(arguments, key)?
            && !value.trim().is_empty()
        {
            return Ok(value);
        }
    }
    Err(ToolExecutionError::InvalidInput {
        reason: format!("Tool `{tool_name}` requires one of: {}", keys.join(" or ")),
    })
}

fn optional_string(arguments: &Value, name: &str) -> Result<Option<String>, ToolExecutionError> {
    let Some(value) = arguments.get(name) else {
        return Ok(None);
    };

    let Some(value) = value.as_str() else {
        return Err(ToolExecutionError::InvalidInput {
            reason: format!("Argument `{name}` must be a string"),
        });
    };

    if value.trim().is_empty() {
        return Err(ToolExecutionError::InvalidInput {
            reason: format!("Argument `{name}` must be a non-empty string"),
        });
    }

    Ok(Some(value.to_string()))
}

fn optional_bool(arguments: &Value, name: &str) -> Result<Option<bool>, ToolExecutionError> {
    let Some(value) = arguments.get(name) else {
        return Ok(None);
    };

    value
        .as_bool()
        .map(Some)
        .ok_or_else(|| ToolExecutionError::InvalidInput {
            reason: format!("Argument `{name}` must be a boolean"),
        })
}

fn optional_i64(arguments: &Value, name: &str) -> Result<Option<i64>, ToolExecutionError> {
    match arguments.get(name) {
        Some(value) => value
            .as_i64()
            .map(Some)
            .ok_or_else(|| ToolExecutionError::InvalidInput {
                reason: format!("Argument `{name}` must be an integer"),
            }),
        None => Ok(None),
    }
}

fn required_string_array(
    arguments: &Value,
    names: &[&str],
    tool_name: &str,
) -> Result<Vec<String>, ToolExecutionError> {
    for name in names {
        let Some(values) = optional_string_array(arguments, name)? else {
            continue;
        };

        if values.is_empty() {
            return Err(ToolExecutionError::InvalidInput {
                reason: format!(
                    "Tool `{tool_name}` requires non-empty string array argument `{name}`"
                ),
            });
        }

        return Ok(values);
    }

    Err(ToolExecutionError::InvalidInput {
        reason: format!(
            "Tool `{tool_name}` requires non-empty string array argument `{}`",
            names.join("` or `")
        ),
    })
}

fn optional_string_array(
    arguments: &Value,
    name: &str,
) -> Result<Option<Vec<String>>, ToolExecutionError> {
    let Some(value) = arguments.get(name) else {
        return Ok(None);
    };

    if let Some(values) = value.as_array() {
        return validate_string_values(
            name,
            values
                .iter()
                .map(|value| {
                    value.as_str().map(str::to_string).ok_or_else(|| {
                        ToolExecutionError::InvalidInput {
                            reason: format!("Argument `{name}` must contain only strings"),
                        }
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
        )
        .map(Some);
    }

    let Some(value) = value.as_str() else {
        return Err(ToolExecutionError::InvalidInput {
            reason: format!("Argument `{name}` must be a string array"),
        });
    };

    if let Ok(values) = serde_json::from_str::<Vec<Value>>(value) {
        return validate_string_values(
            name,
            values
                .into_iter()
                .map(|value| {
                    value.as_str().map(str::to_string).ok_or_else(|| {
                        ToolExecutionError::InvalidInput {
                            reason: format!("Argument `{name}` must contain only strings"),
                        }
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
        )
        .map(Some);
    }

    validate_string_values(name, vec![value.to_string()]).map(Some)
}

fn validate_string_values(
    name: &str,
    values: Vec<String>,
) -> Result<Vec<String>, ToolExecutionError> {
    if values.iter().any(|value| value.trim().is_empty()) {
        return Err(ToolExecutionError::InvalidInput {
            reason: format!("Argument `{name}` must contain only non-empty strings"),
        });
    }

    Ok(values)
}

pub(crate) fn sanitize_function_name(name: &str) -> String {
    let mut sanitized = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                ch.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>();

    while sanitized.contains("__") {
        sanitized = sanitized.replace("__", "_");
    }

    sanitized = sanitized.trim_matches('_').to_string();
    if sanitized.is_empty() {
        "tool".to_string()
    } else if sanitized
        .chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_')
    {
        sanitized
    } else {
        format!("tool_{sanitized}")
    }
}

pub(crate) fn unique_tool_name(
    routes: &HashMap<String, LocalRuntimeToolRoute>,
    preferred_name: &str,
) -> String {
    if !routes.contains_key(preferred_name) {
        return preferred_name.to_string();
    }

    for index in 2.. {
        let candidate = format!("{preferred_name}_{index}");
        if !routes.contains_key(&candidate) {
            return candidate;
        }
    }

    unreachable!("unbounded sequence must find a unique tool name")
}

#[cfg(test)]
#[path = "tool_proto_tests.rs"]
mod tests;
