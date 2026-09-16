/*!
 * @file HtmlToMarkdown
 * @description Lenient single-pass HTML to Markdown conversion for web_fetch.
 *
 * Responsibilities:
 * - Turn structural markup (headings, lists, links, emphasis, code) into
 *   Markdown that reads well in model context.
 * - Drop pure presentation (scripts, styles, attributes, comments).
 * - Degrade malformed or truncated HTML to plain text, never fail.
 *
 * This module must not depend on: I/O, network, or any other workspace
 * crate. It is pure string transformation and unit-testable as such.
 */

//! Minimal HTML → Markdown conversion for the `web_fetch` tool.
//!
//! Goal: make fetched pages readable in model context — structural markup
//! (headings, lists, links, emphasis, code blocks) becomes Markdown, and
//! everything that is pure presentation (scripts, styles, attributes) is
//! dropped. This is deliberately a lenient single-pass converter, not a
//! browser-grade parser: malformed / truncated HTML (a routine web_fetch
//! outcome, given the size cap) degrades to plain text instead of failing.
//!
//! Scope decisions:
//! - `script` / `style` / `head` / `template` content is dropped entirely.
//! - Unknown tags keep their inner text (inline semantics) — the safe
//!   default for custom elements and unknown-but-harmless markup.
//! - Relative `href` values are emitted as-is (no base-URL resolution);
//!   link text still carries the meaning.
//! - Named and numeric character entities are decoded; unknown entities
//!   stay literal (never silently swallowed).

/// One lexical token of the HTML stream.
#[derive(Debug, Clone, PartialEq)]
enum Token {
    /// Character data between tags (entity-decoded).
    Text(String),
    /// An opening tag `<name attr="v">`.
    Open(String, String),
    /// A closing tag `</name>`.
    Close(String),
}

/// Tokenize HTML into text / open / close events.
///
/// - Comments (`<!-- ... -->`) and doctypes are dropped.
/// - A truncated trailing tag (no closing `>`) is dropped whole: emitting a
///   half tag as text would leak markup into the output.
fn tokenize(html: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let bytes = html.as_bytes();
    let mut pos = 0usize;
    let mut text_start = pos;
    // Set when a truncated tag / unterminated comment ends the scan: the
    // trailing bytes are the dropped fragment itself, never trailing text.
    let mut drop_tail = false;
    let flush = |tokens: &mut Vec<Token>, src: &str, start: usize, end: usize| {
        if end > start {
            let raw = &src[start..end];
            // Whitespace-only runs carry no meaning outside `pre`; the
            // renderer re-inserts the single spaces it needs.
            if raw.chars().any(|c| !c.is_whitespace()) {
                tokens.push(Token::Text(decode_entities(raw)));
            }
        }
    };
    while pos < bytes.len() {
        // Comments are handled before the generic tag scan: their content
        // may contain `>`, which would end the tag scan early.
        if html[pos..].starts_with("<!--") {
            flush(&mut tokens, html, text_start, pos);
            match html[pos + 4..].find("-->") {
                Some(off) => {
                    pos = pos + 4 + off + 3;
                    text_start = pos;
                }
                // Unterminated comment: the rest of the input is dropped
                // (truncated), never emitted as text.
                None => {
                    drop_tail = true;
                    break;
                }
            }
            continue;
        }
        if bytes[pos] == b'<' {
            // Find the tag end, honoring quotes inside attribute values.
            let mut i = pos + 1;
            let mut quote: Option<u8> = None;
            let end = loop {
                if i >= bytes.len() {
                    break usize::MAX; // truncated tag: drop it
                }
                let b = bytes[i];
                if let Some(q) = quote {
                    if b == q {
                        quote = None;
                    }
                } else if b == b'"' || b == b'\'' {
                    quote = Some(b);
                } else if b == b'>' {
                    break i;
                }
                i += 1;
            };
            if end == usize::MAX {
                // Truncated trailing tag: the text before it is kept, the
                // partial markup itself is dropped (never emitted as text).
                flush(&mut tokens, html, text_start, pos);
                drop_tail = true;
                break;
            }
            flush(&mut tokens, html, text_start, pos);
            let inner = &html[pos + 1..end];
            if inner.starts_with('!') || inner.starts_with('?') {
                // Doctype / processing instruction: dropped.
                pos = end + 1;
            } else if let Some(name) = inner.strip_prefix('/') {
                tokens.push(Token::Close(tag_name(name)));
                pos = end + 1;
            } else {
                let (name, attrs) = split_tag(inner);
                tokens.push(Token::Open(name, attrs));
                pos = end + 1;
            }
            text_start = pos;
        } else {
            // Advance by one full character so `pos` stays on a char
            // boundary for the `html[pos..]` slicing above.
            pos += html[pos..].chars().next().map_or(1, char::len_utf8);
        }
    }
    // Trailing text after the last tag; skipped entirely when the scan
    // ended on a dropped fragment (the tail IS the fragment).
    if !drop_tail {
        flush(&mut tokens, html, text_start, html.len());
    }
    tokens
}

/// Extract the lowercase tag name from tag-inner text (strips attributes).
fn tag_name(inner: &str) -> String {
    let name: String = inner
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    name
}

/// Split `<name attrs>` into lowercase name and raw attribute string.
fn split_tag(inner: &str) -> (String, String) {
    let trimmed = inner.trim();
    let name_len = trimmed
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .count();
    let name = trimmed[..name_len].to_ascii_lowercase();
    let attrs = trimmed[name_len..].trim().to_string();
    (name, attrs)
}

/// Pull an attribute value out of a raw attribute string (first match,
/// double / single quotes or unquoted).
fn attr_value(attrs: &str, name: &str) -> Option<String> {
    let lower = attrs.to_ascii_lowercase();
    let key = format!("{name}=");
    // `name=` must start at an attribute-name boundary, else a lookup for
    // `href` would be satisfied by `data-href=`.
    let mut from = 0usize;
    let idx = loop {
        let found = lower[from..].find(&key)? + from;
        let at_boundary = found == 0
            || !lower[..found]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.'));
        if at_boundary {
            break found;
        }
        from = found + 1;
    };
    let rest = attrs[idx + key.len()..].trim_start();
    let quote = rest.chars().next()?;
    if quote == '"' || quote == '\'' {
        let end = rest[1..].find(quote)?;
        Some(rest[1..1 + end].to_string())
    } else {
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        Some(rest[..end].to_string())
    }
}

/// Decode common named entities plus decimal / hex numeric references.
/// Unknown entities are kept literal.
fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        let semi = tail.find(';').unwrap_or(tail.len());
        let candidate = &tail[1..semi];
        let decoded = match candidate {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some('\u{00A0}'),
            "copy" => Some('©'),
            "mdash" => Some('—'),
            "ndash" => Some('–'),
            "hellip" => Some('…'),
            "lsquo" => Some('‘'),
            "rsquo" => Some('’'),
            "ldquo" => Some('“'),
            "rdquo" => Some('”'),
            _ => {
                if let Some(num) = candidate.strip_prefix('#') {
                    let code = if let Some(hex) = num.strip_prefix(['x', 'X']) {
                        u32::from_str_radix(hex, 16).ok()
                    } else {
                        num.parse::<u32>().ok()
                    };
                    // Control characters (NUL, ESC, the C1 range) stay
                    // literal so they cannot reach model context;
                    // whitespace such as tab / newline still decodes.
                    code.and_then(char::from_u32)
                        .filter(|c| !c.is_control() || c.is_whitespace())
                } else {
                    None
                }
            }
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &tail[semi + 1..];
            }
            // Unknown or malformed: keep the ampersand literal and move on.
            None => {
                out.push('&');
                rest = &rest[amp + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Tags whose entire content is dropped.
const DROP_WITH_CONTENT: &[&str] = &["script", "style", "head", "template", "noscript"];
/// Convert an HTML document to Markdown.
///
/// Lenient by design: unknown tags pass their text through, truncated
/// markup degrades to text, and output never contains the input's
/// attribute noise. Consecutive blank lines collapse to one.
pub fn html_to_markdown(html: &str) -> String {
    let tokens = tokenize(html);
    let mut out = String::with_capacity(html.len() / 2);
    let mut drop_depth = 0usize; // >0 while inside script/style/head/...
    let mut list_stack: Vec<(bool, u32)> = Vec::new(); // (ordered, next index)
    let mut link_stack: Vec<String> = Vec::new(); // open <a href> targets
    let mut in_pre = false;
    let mut quote_depth = 0usize;

    for token in tokens {
        match token {
            Token::Text(text) => {
                if drop_depth > 0 {
                    continue;
                }
                let text = if in_pre { text } else { collapse_ws(&text) };
                if text.is_empty() {
                    continue;
                }
                if quote_depth > 0 && (out.is_empty() || out.ends_with('\n')) {
                    out.push_str("> ");
                }
                out.push_str(&text);
            }
            Token::Open(name, attrs) => {
                if DROP_WITH_CONTENT.contains(&name.as_str()) {
                    drop_depth += 1;
                    continue;
                }
                if drop_depth > 0 {
                    continue;
                }
                match name.as_str() {
                    "br" => out.push_str("  \n"),
                    "hr" => out.push_str("\n---\n"),
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                        let level = name[1..].parse::<usize>().unwrap_or(6);
                        ensure_break(&mut out);
                        out.push_str(&"#".repeat(level));
                        out.push(' ');
                    }
                    "ul" => list_stack.push((false, 1)),
                    "ol" => list_stack.push((true, 1)),
                    // Compact lists: the item marker starts on the next
                    // line, no blank line between items.
                    "li" => {
                        if !out.is_empty() && !out.ends_with('\n') {
                            out.push('\n');
                        }
                        match list_stack.last_mut() {
                            Some((true, idx)) => {
                                out.push_str(&format!("{idx}. "));
                                *idx += 1;
                            }
                            _ => out.push_str("- "),
                        }
                    }
                    "pre" => {
                        ensure_break(&mut out);
                        out.push_str("```\n");
                        in_pre = true;
                    }
                    "blockquote" => {
                        ensure_break(&mut out);
                        quote_depth += 1;
                    }
                    "a" => {
                        // Only real links become Markdown links; `#anchor`
                        // jumps carry no meaning in a fetched snapshot.
                        if let Some(href) = attr_value(&attrs, "href")
                            && !href.starts_with('#')
                        {
                            out.push('[');
                            link_stack.push(href);
                        }
                    }
                    "strong" | "b" => out.push_str("**"),
                    "em" | "i" => out.push('*'),
                    "code" if !in_pre => out.push('`'),
                    _ => {}
                }
            }
            Token::Close(name) => {
                if DROP_WITH_CONTENT.contains(&name.as_str()) {
                    drop_depth = drop_depth.saturating_sub(1);
                    continue;
                }
                if drop_depth > 0 {
                    continue;
                }
                match name.as_str() {
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => out.push('\n'),
                    "ul" | "ol" => {
                        list_stack.pop();
                        ensure_break(&mut out);
                    }
                    // Compact lists: items separate with a single newline,
                    // only the list end forces a blank line.
                    "li" => out.push('\n'),
                    "pre" => {
                        in_pre = false;
                        out.push_str("\n```\n");
                    }
                    "blockquote" => quote_depth = quote_depth.saturating_sub(1),
                    "a" => {
                        if let Some(href) = link_stack.pop() {
                            out.push_str(&format!("]({href})"));
                        }
                    }
                    "p" | "div" | "section" | "article" | "header" | "footer" | "main"
                    | "aside" | "nav" | "table" | "tr" | "form" | "title" => out.push('\n'),
                    "strong" | "b" => out.push_str("**"),
                    "em" | "i" => out.push('*'),
                    "code" if !in_pre => out.push('`'),
                    _ => {}
                }
            }
        }
    }

    // Collapse runs of blank lines and trim; a leading newline (from the
    // first block tag) must not survive.
    let mut collapsed = String::with_capacity(out.len());
    let mut blank = false;
    for line in out.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            if blank || collapsed.is_empty() {
                continue;
            }
            blank = true;
        } else {
            blank = false;
        }
        collapsed.push_str(line);
        collapsed.push('\n');
    }
    collapsed.trim_end().to_string()
}

/// Ensure the output ends with exactly one blank line before a block starts.
fn ensure_break(out: &mut String) {
    while !out.is_empty() && (out.ends_with('\n') || out.ends_with(' ')) {
        out.pop();
    }
    if !out.is_empty() {
        out.push_str("\n\n");
    }
}

/// Collapse internal whitespace runs to single spaces (outside `pre`).
fn collapse_ws(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            pending_space = !out.is_empty();
        } else {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headings_paragraphs_and_lists() {
        let html = "<html><head><title>T</title></head><body>\
                    <h1>Title</h1><p>First   paragraph</p>\
                    <ul><li>one</li><li>two</li></ul>\
                    <ol><li>first</li><li>second</li></ol>\
                    </body></html>";
        let md = html_to_markdown(html);
        assert!(md.contains("# Title"), "{md}");
        assert!(md.contains("First paragraph"), "{md}");
        assert!(md.contains("- one\n- two"), "{md}");
        assert!(md.contains("1. first\n2. second"), "{md}");
        assert!(!md.contains("<html>"), "tags must not leak: {md}");
        assert!(!md.contains("T\n"), "head content must be dropped: {md}");
    }

    #[test]
    fn scripts_styles_and_comments_are_dropped() {
        let html = "<p>keep</p><script>var x = '<p>evil</p>';</script>\
                    <style>.p{color:red}</style><!-- comment --><p>after</p>";
        let md = html_to_markdown(html);
        assert!(md.contains("keep") && md.contains("after"), "{md}");
        assert!(!md.contains("evil") && !md.contains("color"), "{md}");
        assert!(!md.contains("comment"), "{md}");
    }

    #[test]
    fn links_emphasis_and_code() {
        let html = "<p>see <a href=\"https://e.com/x\">the docs</a> now</p>\
                    <p><strong>bold</strong> and <em>ital</em> and <code>x=1</code></p>\
                    <pre><code>fn main() {\n    let a = 1;   // kept\n}</code></pre>";
        let md = html_to_markdown(html);
        assert!(md.contains("[the docs](https://e.com/x)"), "{md}");
        assert!(
            md.contains("**bold**") && md.contains("*ital*") && md.contains("`x=1`"),
            "{md}"
        );
        assert!(
            md.contains("let a = 1;   // kept"),
            "pre preserves ws: {md}"
        );
    }

    #[test]
    fn entities_decode_and_unknown_stay_literal() {
        assert_eq!(
            html_to_markdown("<p>a &amp; b &lt;c&gt; &#65; &#x42; &nope;</p>"),
            "a & b <c> A B &nope;"
        );
        assert_eq!(html_to_markdown("<p>caf&#233; &mdash; ok</p>"), "café — ok");
    }

    #[test]
    fn attribute_lookup_requires_name_boundary() {
        // `data-href` must not satisfy an `href` lookup.
        let md = html_to_markdown("<a data-href=\"https://e.com/x\">text</a>");
        assert_eq!(md, "text");
        // A decoy attribute is skipped in favor of the real one.
        let md = html_to_markdown(
            "<a data-href=\"decoy\" title=\"t\" href=\"https://e.com/real\">link</a>",
        );
        assert!(md.contains("[link](https://e.com/real)"), "{md}");
        assert!(!md.contains("decoy"), "{md}");
    }

    #[test]
    fn multibyte_text_does_not_panic() {
        // Regression: the tokenizer once advanced `pos` by single bytes and
        // panicked when it landed inside a multi-byte character.
        assert_eq!(html_to_markdown("<p>café</p>"), "café");
        assert_eq!(html_to_markdown("<p>你好世界</p>"), "你好世界");
        assert_eq!(html_to_markdown("<p>🦀 push</p>"), "🦀 push");
        // Multi-byte text running into a tag.
        assert_eq!(html_to_markdown("中文<code>x</code>"), "中文`x`");
        // Multi-byte text running into a truncated tag.
        assert_eq!(html_to_markdown("中文<div class=\"x"), "中文");
    }

    #[test]
    fn numeric_control_references_stay_literal() {
        // NUL and other control characters must not enter the output;
        // whitespace references still decode.
        assert_eq!(html_to_markdown("<p>x &#0; y</p>"), "x &#0; y");
        assert_eq!(html_to_markdown("<p>x &#1; y</p>"), "x &#1; y");
        assert_eq!(html_to_markdown("<p>x &#x1F; y</p>"), "x &#x1F; y");
        assert_eq!(html_to_markdown("<p>a&#9;b</p>"), "a b");
    }

    #[test]
    fn truncated_and_malformed_markup_degrades_to_text() {
        // Truncated trailing tag is dropped, preceding text kept.
        assert_eq!(html_to_markdown("<p>hello</p><div class=\"x"), "hello");
        // Unterminated comment drops the rest.
        assert_eq!(html_to_markdown("<p>a</p><!-- never closed"), "a");
        // Unknown tags keep their text.
        assert_eq!(html_to_markdown("<custom>text</custom>"), "text");
        // Plain text passes through untouched.
        assert_eq!(html_to_markdown("just words"), "just words");
    }

    #[test]
    fn blockquotes_and_nested_lists() {
        let html = "<blockquote><p>wisdom</p></blockquote>\
                    <ul><li>a<ul><li>a1</li></ul></li><li>b</li></ul>";
        let md = html_to_markdown(html);
        assert!(md.contains("> wisdom"), "{md}");
        assert!(md.contains("- a"), "{md}");
        assert!(md.contains("- a1"), "{md}");
        assert!(md.contains("- b"), "{md}");
    }
}
