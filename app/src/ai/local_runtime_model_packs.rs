//! Model-family prompt/tool-schema packs for the local OpenAI-compatible runtime.
//!
//! Detects a coarse family from the configured model id and applies short prompt
//! addenda plus light tool-description tweaks. Generic models keep today's
//! behavior (no addendum; identity schema tweaks).

use local_agent_runtime::ToolSchema;

/// Coarse local-model family used to select prompt/schema packs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ModelFamily {
    Qwen,
    DeepSeek,
    Llama,
    #[default]
    Generic,
}

/// Detect a model family from an Ollama / OpenAI-compatible model id.
///
/// Matching is case-insensitive substring based (after trimming). Tags such as
/// `:latest` are left in place; they do not affect the heuristics.
pub fn detect_model_family(model_id: &str) -> ModelFamily {
    let id = model_id.trim().to_ascii_lowercase();
    if id.is_empty() {
        return ModelFamily::Generic;
    }
    // Order matters: more specific prefixes first when they could overlap.
    if id.contains("deepseek") {
        ModelFamily::DeepSeek
    } else if id.contains("qwen") {
        ModelFamily::Qwen
    } else if id.contains("llama") || id.contains("codellama") {
        ModelFamily::Llama
    } else {
        ModelFamily::Generic
    }
}

impl ModelFamily {
    /// Stable marker used in prompts/tests to identify which pack was applied.
    pub fn section_marker(self) -> Option<&'static str> {
        match self {
            Self::Qwen => Some("## Model Pack: Qwen"),
            Self::DeepSeek => Some("## Model Pack: DeepSeek"),
            Self::Llama => Some("## Model Pack: Llama"),
            Self::Generic => None,
        }
    }

    /// Short family-specific addendum appended after the shared system prompt.
    pub fn prompt_addendum(self) -> Option<&'static str> {
        match self {
            Self::Qwen => Some(
                "## Model Pack: Qwen\n\
Prefer native OpenAI-style structured tool_calls over prose. \
If you must emit tools as text, use the recoverable form \
`<function=TOOL_NAME>` with `<parameter=NAME>value</parameter>` blocks \
(and optional surrounding `<tool_call>` tags) — never invent a different XML dialect. \
After a TOOL_SUCCESS result, answer the user from that output; do not re-run the same shell command \
or invent timeouts when exit_code is 0.",
            ),
            Self::DeepSeek => Some(
                "## Model Pack: DeepSeek\n\
When an action is required, emit a real function/tool call with valid JSON arguments — \
do not only describe the command in markdown. Keep argument values JSON-safe \
(escape quotes; use objects/arrays as the schema requires). \
Prefer one focused tool call at a time unless parallel read-only tools clearly help.",
            ),
            Self::Llama => Some(
                "## Model Pack: Llama\n\
Use the provider tool/function-calling interface for actions; do not put tool calls inside \
markdown code fences or pretend a fenced JSON block was executed. \
Follow each tool schema exactly (required fields, types). \
After tools return, summarize from the tool output rather than guessing.",
            ),
            Self::Generic => None,
        }
    }

    /// Light description prefixes for a fixed allowlist of tools. Parameter shapes unchanged.
    pub fn schema_description_prefix(self, tool_name: &str) -> Option<&'static str> {
        match (self, tool_name) {
            (Self::Qwen, "edit_files") => Some(
                "[Qwen] You MUST call this tool to change files — never shell redirects. ",
            ),
            (Self::Qwen, "run_shell_command") => Some(
                "[Qwen] Prefer structured tool_calls; set is_read_only=true for read-only commands. ",
            ),
            (Self::Qwen, "update_todos") => Some(
                "[Qwen] REPLACE the full pending list (omit ids to drop them; do not merge). ",
            ),
            (Self::DeepSeek, "edit_files") => Some(
                "[DeepSeek] Call with JSON matching the schema; do not narrate edits without calling. ",
            ),
            (Self::DeepSeek, "run_shell_command") => Some(
                "[DeepSeek] Emit a real tool call with a JSON `command` string. ",
            ),
            (Self::DeepSeek, "update_todos") => Some(
                "[DeepSeek] Pass a full `todos` array that replaces pending (not a merge). ",
            ),
            (Self::Llama, "edit_files") => Some(
                "[Llama] Use function calling with an `edits` array — not markdown patches. ",
            ),
            (Self::Llama, "run_shell_command") => Some(
                "[Llama] Use a tool call, not a fenced shell block. ",
            ),
            (Self::Llama, "update_todos") => Some(
                "[Llama] Replace the entire pending todo list via this tool. ",
            ),
            (Self::Generic, _) => None,
            (_, _) => None,
        }
    }
}

/// Append the family addendum when present.
pub fn append_prompt_addendum(prompt: &mut String, family: ModelFamily) {
    if let Some(addendum) = family.prompt_addendum() {
        prompt.push_str("\n\n");
        prompt.push_str(addendum);
        prompt.push('\n');
    }
}

/// Apply pack-only description prefixes; names and parameter schemas stay identical.
pub fn apply_schema_tweaks(family: ModelFamily, mut schemas: Vec<ToolSchema>) -> Vec<ToolSchema> {
    if family == ModelFamily::Generic {
        return schemas;
    }
    for schema in &mut schemas {
        if let Some(prefix) = family.schema_description_prefix(&schema.name) {
            if !schema.description.starts_with(prefix) {
                schema.description = format!("{}{}", prefix, schema.description);
            }
        }
    }
    schemas
}

#[cfg(test)]
#[path = "local_runtime_model_packs_tests.rs"]
mod tests;
