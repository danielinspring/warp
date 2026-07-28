use local_agent_runtime::provider::text_tool_calls::extract_qwen_style_tool_calls;

use super::normalize_base_url;

#[test]
fn recovers_qwen_xml_shell_command_from_assistant_text() {
    let text = r#"1부터 100까지의 합을 계산하기 위해 Python 코드를 실행할게요.

<function=run_shell_command>
<parameter=command>
python -c "print(sum(range(1, 101)))"
</parameter>
</function>
</tool_call>"#;

    let (cleaned, calls) = extract_qwen_style_tool_calls(text);
    assert!(!cleaned.contains("<function="));
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "run_shell_command");
    assert_eq!(
        calls[0].arguments["command"],
        "python -c \"print(sum(range(1, 101)))\""
    );
}

#[test]
fn normalize_base_url_strips_trailing_slash_and_v1() {
    assert_eq!(
        normalize_base_url("http://localhost:11434/"),
        "http://localhost:11434"
    );
    assert_eq!(
        normalize_base_url("http://100.95.111.65:4000/v1"),
        "http://100.95.111.65:4000"
    );
    assert_eq!(
        normalize_base_url("http://100.95.111.65:4000/v1/"),
        "http://100.95.111.65:4000"
    );
    assert_eq!(
        normalize_base_url("  http://localhost:11434/v1  "),
        "http://localhost:11434"
    );
}

#[test]
fn normalize_base_url_preserves_non_v1_paths() {
    assert_eq!(
        normalize_base_url("http://localhost:11434/ollama"),
        "http://localhost:11434/ollama"
    );
}
