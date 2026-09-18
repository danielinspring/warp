use local_agent_runtime::ToolSchema;

use super::{apply_schema_tweaks, detect_model_family, ModelFamily};

#[test]
fn detect_model_family_table() {
    assert_eq!(detect_model_family("qwen3-coder:latest"), ModelFamily::Qwen);
    assert_eq!(detect_model_family("Qwen2.5-Coder"), ModelFamily::Qwen);
    assert_eq!(
        detect_model_family("deepseek-coder-v2"),
        ModelFamily::DeepSeek
    );
    assert_eq!(
        detect_model_family("deepseek-r1:14b"),
        ModelFamily::DeepSeek
    );
    assert_eq!(detect_model_family("llama3.1:8b"), ModelFamily::Llama);
    assert_eq!(detect_model_family("codellama:13b"), ModelFamily::Llama);
    assert_eq!(detect_model_family("gpt-oss"), ModelFamily::Generic);
    assert_eq!(detect_model_family(""), ModelFamily::Generic);
    assert_eq!(detect_model_family("   "), ModelFamily::Generic);
}

#[test]
fn prompt_addenda_have_stable_markers() {
    assert!(ModelFamily::Qwen
        .prompt_addendum()
        .unwrap()
        .contains("## Model Pack: Qwen"));
    assert!(ModelFamily::DeepSeek
        .prompt_addendum()
        .unwrap()
        .contains("## Model Pack: DeepSeek"));
    assert!(ModelFamily::Llama
        .prompt_addendum()
        .unwrap()
        .contains("## Model Pack: Llama"));
    assert!(ModelFamily::Generic.prompt_addendum().is_none());
    assert!(ModelFamily::Generic.section_marker().is_none());
}

#[test]
fn schema_tweaks_prefix_allowlisted_tools_only() {
    let schemas = vec![
        ToolSchema {
            name: "edit_files".into(),
            description: "Edit files.".into(),
            parameters: serde_json::json!({}),
        },
        ToolSchema {
            name: "grep".into(),
            description: "Search.".into(),
            parameters: serde_json::json!({}),
        },
    ];

    let tweaked = apply_schema_tweaks(ModelFamily::Qwen, schemas.clone());
    assert!(tweaked[0].description.starts_with("[Qwen]"));
    assert!(tweaked[0].description.contains("Edit files."));
    assert_eq!(tweaked[0].parameters, schemas[0].parameters);
    assert_eq!(tweaked[1].description, "Search.");

    let generic = apply_schema_tweaks(ModelFamily::Generic, schemas);
    assert_eq!(generic[0].description, "Edit files.");
}
