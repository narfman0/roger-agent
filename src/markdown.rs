use pulldown_cmark::{html, Options, Parser};

/// Render a complete markdown string to HTML.
pub fn to_html(md: &str) -> String {
    let opts = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES;
    let parser = Parser::new_ext(md, opts);
    let mut out = String::with_capacity(md.len() * 2);
    html::push_html(&mut out, parser);
    out
}

/// Returns the byte offset just past the last `\n\n` that falls *outside* a
/// code fence, or `None` if no such boundary exists.  Only text before this
/// offset is safe to render as HTML — later tokens cannot affect it.
pub fn last_safe_paragraph_end(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut in_fence = false;
    let mut last_safe: Option<usize> = None;
    let mut i = 0;
    while i < bytes.len() {
        // Detect ``` at start of a line (or start of string).
        if bytes[i..].starts_with(b"```") && (i == 0 || bytes[i - 1] == b'\n') {
            in_fence = !in_fence;
            i += 3;
            continue;
        }
        if !in_fence && bytes[i..].starts_with(b"\n\n") {
            last_safe = Some(i + 2);
            i += 2;
            continue;
        }
        i += 1;
    }
    last_safe
}

/// Build the `formatted_body` for a partially-streamed message.
///
/// `committed_html` is the already-rendered HTML for completed paragraphs.
/// `tail` is the in-progress text that hasn't hit a paragraph boundary yet.
/// It is HTML-escaped and appended as a plain `<p>` so it renders legibly
/// without risking broken markdown tags.
pub fn streaming_formatted_body(committed_html: &str, tail: &str) -> String {
    if tail.trim().is_empty() {
        return committed_html.to_string();
    }
    let escaped = html_escape(tail);
    if committed_html.is_empty() {
        format!("<p>{}</p>", escaped)
    } else {
        format!("{}<p>{}</p>", committed_html, escaped)
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\n', "<br>")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paragraph_boundary_outside_fence() {
        let text = "hello world\n\nNext paragraph";
        assert_eq!(last_safe_paragraph_end(text), Some(13));
    }

    #[test]
    fn double_newline_inside_fence_ignored() {
        let text = "```rust\nfn foo() {}\n\nfn bar() {}\n```\n\nafter";
        // The \n\n inside the fence must not be returned; only the one after ``` counts.
        let idx = last_safe_paragraph_end(text).unwrap();
        assert!(text[..idx].contains("after") || idx > text.find("```\n\nafter").unwrap());
    }

    #[test]
    fn no_boundary_returns_none() {
        assert_eq!(last_safe_paragraph_end("single line"), None);
    }

    #[test]
    fn to_html_renders_bold() {
        let html = to_html("**bold**");
        assert!(html.contains("<strong>bold</strong>"));
    }
}
