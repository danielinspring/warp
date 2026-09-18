//! Lightweight HTML → plain text extraction for local web_fetch.

/// Extract a best-effort page title and plain text body from HTML.
pub fn html_to_text(html: &str) -> (String, String) {
    let title = extract_title(html);
    let without_noise = strip_script_and_style(html);
    let text = collapse_whitespace(&strip_tags(&without_noise));
    (title, text)
}

fn extract_title(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let Some(start) = lower.find("<title") else {
        return String::new();
    };
    let after = &html[start..];
    let Some(gt) = after.find('>') else {
        return String::new();
    };
    let content = &after[gt + 1..];
    let content_lower = content.to_ascii_lowercase();
    let Some(end) = content_lower.find("</title>") else {
        return String::new();
    };
    collapse_whitespace(&decode_basic_entities(&content[..end]))
}

fn strip_script_and_style(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let lower = html.to_ascii_lowercase();
    let mut i = 0;
    while i < html.len() {
        if lower[i..].starts_with("<script") {
            if let Some(rel) = lower[i..].find("</script>") {
                i += rel + "</script>".len();
                continue;
            }
            break;
        }
        if lower[i..].starts_with("<style") {
            if let Some(rel) = lower[i..].find("</style>") {
                i += rel + "</style>".len();
                continue;
            }
            break;
        }
        let ch = html[i..].chars().next().expect("index on char boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => {
                in_tag = true;
                // Treat block-ish tags as paragraph breaks by inserting space when leaving tags
                // is handled below; insert newline for common block ends via heuristic.
            }
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    decode_basic_entities(&out)
}

fn decode_basic_entities(s: &str) -> String {
    s.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
}

fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    out.trim().to_string()
}

/// Truncate extracted text to `max_chars`, returning (text, truncated).
pub fn truncate_text(text: String, max_chars: usize) -> (String, bool) {
    if text.chars().count() <= max_chars {
        return (text, false);
    }
    let truncated: String = text.chars().take(max_chars).collect();
    (format!("{truncated}\n\n[truncated]"), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_title_and_body() {
        let html = r#"
            <html><head><title>Hello &amp; Docs</title>
            <style>body{color:red}</style>
            <script>alert(1)</script>
            </head><body><h1>Intro</h1><p>Useful content.</p></body></html>
        "#;
        let (title, text) = html_to_text(html);
        assert_eq!(title, "Hello & Docs");
        assert!(text.contains("Intro"));
        assert!(text.contains("Useful content"));
        assert!(!text.contains("alert"));
        assert!(!text.contains("color:red"));
    }

    #[test]
    fn truncates_with_marker() {
        let (text, truncated) = truncate_text("abcdefghij".to_string(), 5);
        assert!(truncated);
        assert!(text.starts_with("abcde"));
        assert!(text.contains("[truncated]"));
    }
}
