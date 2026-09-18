use local_agent_runtime::provider::text_tool_calls::extract_qwen_style_tool_calls;

use super::{
    host_prefers_openai_discovery, normalize_base_url, openai_compatible_provider_label,
    url_prefers_openai_discovery, ToolCallFunctionParsed, ToolCallParsed,
};

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

#[test]
fn url_prefers_openai_discovery_and_provider_label() {
    assert!(url_prefers_openai_discovery("http://localhost:1234/v1"));
    assert!(url_prefers_openai_discovery(
        "https://api.groq.com/openai/v1/"
    ));
    assert!(!url_prefers_openai_discovery("http://localhost:11434"));
    assert!(host_prefers_openai_discovery("http://localhost:1234"));
    assert!(!host_prefers_openai_discovery("http://localhost:11434"));
    assert_eq!(
        openai_compatible_provider_label("http://localhost:11434"),
        "Ollama"
    );
    assert_eq!(
        openai_compatible_provider_label("http://100.95.111.65:4000/v1"),
        "OpenAI-compatible"
    );
    assert_eq!(
        openai_compatible_provider_label("https://api.groq.com/openai/v1"),
        "OpenAI-compatible"
    );
}

#[test]
fn parses_tool_call_arguments_from_json_string_or_object() {
    let as_string: ToolCallParsed = serde_json::from_str(
        r#"{"id":"c1","type":"function","function":{"name":"run_shell_command","arguments":"{\"command\":\"pwd\"}"}}"#,
    )
    .unwrap();
    assert_eq!(
        as_string.function.arguments.as_json_string(),
        r#"{"command":"pwd"}"#
    );

    let as_object: ToolCallFunctionParsed =
        serde_json::from_str(r#"{"name":"read_files","arguments":{"paths":["src/lib.rs"]}}"#)
            .unwrap();
    let parsed: serde_json::Value =
        serde_json::from_str(&as_object.arguments.as_json_string()).unwrap();
    assert_eq!(parsed["paths"][0], "src/lib.rs");
}
