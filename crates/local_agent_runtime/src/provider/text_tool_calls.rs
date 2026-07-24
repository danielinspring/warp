//! Recover tool calls that models emit as text instead of OpenAI `tool_calls`.
//!
//! Some Ollama / LiteLLM models (notably Qwen coder variants) ignore structured
//! function calling and write tool invocations like:
//!
//! ```text
//! <function=file_glob_v2>
//! <parameter=patterns>
//! ["**/learn-harness-engineering"]
//! </parameter>
//! </function>
//! </tool_call>
//! ```
//!
//! With `finish_reason: stop` and `tool_calls: null`. Without recovery the
//! runtime treats that as a plain text turn and never executes tools.

use serde_json::Value;

use crate::tools::ToolCall;

/// If `text` contains Qwen-style XML tool calls, return cleaned text plus the
/// parsed calls. Otherwise return the original text and an empty call list.
pub fn extract_qwen_style_tool_calls(text: &str) -> (String, Vec<ToolCall>) {
    let Some(start) = find_tool_markup_start(text) else {
        return (text.to_string(), Vec::new());
    };

    let mut calls = Vec::new();
    let mut cursor = start;
    let prefix = text[..start].trim_end().to_string();

    while cursor < text.len() {
        let slice = &text[cursor..];
        let trimmed = slice.trim_start();
        let leading = slice.len() - trimmed.len();
        cursor += leading;

        if text[cursor..].starts_with("</tool_call>") {
            cursor += "</tool_call>".len();
            continue;
        }
        if text[cursor..].starts_with("<tool_call>") {
            cursor += "<tool_call>".len();
            continue;
        }

        let Some((call, consumed)) = parse_function_block(&text[cursor..]) else {
            break;
        };
        calls.push(call);
        cursor += consumed;
    }

    if calls.is_empty() {
        return (text.to_string(), Vec::new());
    }

    let suffix = text[cursor..].trim_start();
    let cleaned = if suffix.is_empty() {
        prefix
    } else if prefix.is_empty() {
        suffix.to_string()
    } else {
        format!("{prefix}\n{suffix}")
    };

    (cleaned, calls)
}

fn find_tool_markup_start(text: &str) -> Option<usize> {
    let tool_call = text.find("<tool_call>");
    let function = text.find("<function=");
    match (tool_call, function) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn parse_function_block(input: &str) -> Option<(ToolCall, usize)> {
    let rest = input.strip_prefix("<function=")?;
    let name_end = rest.find('>')?;
    let name = rest[..name_end].trim();
    if name.is_empty() {
        return None;
    }
    let after_open = &rest[name_end + 1..];
    let close_tag = "</function>";
    let close_at = after_open.find(close_tag)?;
    let body = &after_open[..close_at];
    let consumed = "<function=".len() + name_end + 1 + close_at + close_tag.len();

    let mut arguments = serde_json::Map::new();
    let mut param_cursor = 0;
    while let Some(rel) = body[param_cursor..].find("<parameter=") {
        let absolute = param_cursor + rel;
        let Some((key, value, param_consumed)) = parse_parameter_block(&body[absolute..]) else {
            break;
        };
        arguments.insert(key, value);
        param_cursor = absolute + param_consumed;
    }

    Some((
        ToolCall {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.to_string(),
            arguments: Value::Object(arguments),
        },
        consumed,
    ))
}

fn parse_parameter_block(input: &str) -> Option<(String, Value, usize)> {
    let rest = input.strip_prefix("<parameter=")?;
    let name_end = rest.find('>')?;
    let key = rest[..name_end].trim();
    if key.is_empty() {
        return None;
    }
    let after_open = &rest[name_end + 1..];
    let close_tag = "</parameter>";
    let close_at = after_open.find(close_tag)?;
    let raw_value = after_open[..close_at].trim();
    let consumed = "<parameter=".len() + name_end + 1 + close_at + close_tag.len();
    let value = parse_parameter_value(raw_value);
    Some((key.to_string(), value, consumed))
}

fn parse_parameter_value(raw: &str) -> Value {
    if raw.is_empty() {
        return Value::String(String::new());
    }
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_function_block_without_opening_tool_call_tag() {
        let text = r#"I'll search for it.

<function=file_glob_v2>
<parameter=patterns>
["**/learn-harness-engineering"]
</parameter>
</function>
</tool_call>"#;

        let (cleaned, calls) = extract_qwen_style_tool_calls(text);
        assert_eq!(cleaned, "I'll search for it.");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "file_glob_v2");
        assert_eq!(
            calls[0].arguments,
            serde_json::json!({"patterns": ["**/learn-harness-engineering"]})
        );
    }

    #[test]
    fn extracts_shell_command_parameter_as_string() {
        let text = r#"<function=run_shell_command>
<parameter=command>
find /Users/lt-018 -type d -name "learn-harness-engineering" 2>/dev/null
</parameter>
</function>
</tool_call>"#;

        let (cleaned, calls) = extract_qwen_style_tool_calls(text);
        assert!(cleaned.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "run_shell_command");
        assert_eq!(
            calls[0].arguments["command"],
            "find /Users/lt-018 -type d -name \"learn-harness-engineering\" 2>/dev/null"
        );
    }

    #[test]
    fn leaves_plain_text_unchanged() {
        let text = "No tools here.";
        let (cleaned, calls) = extract_qwen_style_tool_calls(text);
        assert_eq!(cleaned, text);
        assert!(calls.is_empty());
    }

    #[test]
    fn extracts_multiple_function_blocks() {
        let text = r#"Working.

<tool_call>
<function=read_files>
<parameter=paths>
["a.rs"]
</parameter>
</function>
</tool_call>
<tool_call>
<function=grep>
<parameter=pattern>
foo
</parameter>
</function>
</tool_call>"#;

        let (cleaned, calls) = extract_qwen_style_tool_calls(text);
        assert_eq!(cleaned, "Working.");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "read_files");
        assert_eq!(calls[1].name, "grep");
    }
}
