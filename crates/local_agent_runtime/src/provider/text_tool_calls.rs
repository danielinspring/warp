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
            let after = &text[cursor..];
            // JSON body form: <tool_call>{"name":"...","arguments":{...}}</tool_call>
            if let Some((call, consumed)) = parse_json_tool_call_body(after) {
                calls.push(call);
                cursor += consumed;
                continue;
            }
            continue;
        }

        if let Some((call, consumed)) = parse_function_block(&text[cursor..]) {
            calls.push(call);
            cursor += consumed;
            continue;
        }

        // Skip an unrecognized fragment so a trailing junk tag cannot abort
        // recovery of earlier calls, but stop if we cannot advance.
        break;
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

/// True when `text` still contains raw tool markup that should not be shown
/// as assistant prose once calls have been recovered.
pub fn contains_tool_markup(text: &str) -> bool {
    find_tool_markup_start(text).is_some()
}

fn find_tool_markup_start(text: &str) -> Option<usize> {
    [
        text.find("<tool_call>"),
        text.find("<function="),
        text.find("<function name="),
    ]
    .into_iter()
    .flatten()
    .min()
}

fn parse_function_block(input: &str) -> Option<(ToolCall, usize)> {
    // Qwen: <function=name>…</function>
    // Alt:  <function name="name">…</function>
    let (name, after_open, open_len) = if let Some(rest) = input.strip_prefix("<function=") {
        let name_end = rest.find('>')?;
        let name = rest[..name_end].trim();
        if name.is_empty() {
            return None;
        }
        (name.to_string(), &rest[name_end + 1..], "<function=".len() + name_end + 1)
    } else if let Some(rest) = input.strip_prefix("<function name=\"") {
        let name_end = rest.find('"')?;
        let name = rest[..name_end].trim();
        if name.is_empty() {
            return None;
        }
        let after_name = &rest[name_end + 1..];
        let gt = after_name.find('>')?;
        (
            name.to_string(),
            &after_name[gt + 1..],
            "<function name=\"".len() + name_end + 1 + gt + 1,
        )
    } else {
        return None;
    };

    let close_tag = "</function>";
    let close_at = after_open.find(close_tag)?;
    let body = &after_open[..close_at];
    let consumed = open_len + close_at + close_tag.len();

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
            name,
            arguments: Value::Object(arguments),
        },
        consumed,
    ))
}

fn parse_json_tool_call_body(input: &str) -> Option<(ToolCall, usize)> {
    let trimmed = input.trim_start();
    let leading = input.len() - trimmed.len();
    if !trimmed.starts_with('{') {
        return None;
    }
    let end = find_balanced_json_end(trimmed)?;
    let json_str = &trimmed[..=end];
    let value: Value = serde_json::from_str(json_str).ok()?;
    let name = value
        .get("name")
        .or_else(|| value.pointer("/function/name"))
        .and_then(Value::as_str)?
        .to_string();
    if name.is_empty() {
        return None;
    }
    let arguments = value
        .get("arguments")
        .or_else(|| value.get("parameters"))
        .or_else(|| value.pointer("/function/arguments"))
        .cloned()
        .unwrap_or_else(|| Value::Object(Default::default()));
    let arguments = match arguments {
        Value::String(s) => serde_json::from_str(&s).unwrap_or(Value::String(s)),
        other => other,
    };

    let after_json = &trimmed[end + 1..];
    let after_trim = after_json.trim_start();
    let mut consumed = leading + end + 1 + (after_json.len() - after_trim.len());
    if after_trim.starts_with("</tool_call>") {
        consumed += "</tool_call>".len();
    }

    Some((
        ToolCall {
            id: uuid::Uuid::new_v4().to_string(),
            name,
            arguments,
        },
        consumed,
    ))
}

fn find_balanced_json_end(input: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escape = false;
    for (idx, ch) in input.char_indices() {
        if in_string {
            if escape {
                escape = false;
            } else if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(idx);
                }
            }
            _ => {}
        }
    }
    None
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

    #[test]
    fn extracts_python_sum_shell_command_sample() {
        let text = r#"1부터 100까지의 합을 계산하기 위해 Python 코드를 실행할게요.

<function=run_shell_command>
<parameter=command>
python -c "print(sum(range(1, 101)))"
</parameter>
</function>
</tool_call>"#;

        let (cleaned, calls) = extract_qwen_style_tool_calls(text);
        assert_eq!(
            cleaned,
            "1부터 100까지의 합을 계산하기 위해 Python 코드를 실행할게요."
        );
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "run_shell_command");
        assert_eq!(
            calls[0].arguments["command"],
            "python -c \"print(sum(range(1, 101)))\""
        );
    }

    #[test]
    fn extracts_json_tool_call_body() {
        let text = r#"Running.

<tool_call>
{"name":"run_shell_command","arguments":{"command":"echo hi","is_read_only":true}}
</tool_call>"#;

        let (cleaned, calls) = extract_qwen_style_tool_calls(text);
        assert_eq!(cleaned, "Running.");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "run_shell_command");
        assert_eq!(calls[0].arguments["command"], "echo hi");
        assert_eq!(calls[0].arguments["is_read_only"], true);
    }
}
