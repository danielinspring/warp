use super::normalize_base_url;

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
