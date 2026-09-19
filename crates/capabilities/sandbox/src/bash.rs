//! AST-based command extraction for permission matching.
//!
//! String segmentation (`split_command_segments`) cuts on separators
//! without quoting awareness: `echo "a; curl evil"` yields a fake
//! `curl evil" ` segment (false deny), and `X=1 curl evil` yields a
//! segment the `curl *` prefix never matches (false allow). Parsing the
//! command with tree-sitter-bash fixes both. Each parsed `command` node
//! contributes **two** segments: its full text (information-preserving,
//! so literal deny patterns keep matching) and its "bare" text with
//! leading `variable_assignment` children dropped and word spacing
//! normalized (`X=1 curl evil` -> also `curl evil`), so name-prefixed
//! rules match behind env prefixes.
//!
//! Failure is always conservative: a parse error or an `ERROR` node
//! yields `None`, and the caller falls back to string segmentation
//! (deny coverage then matches the pre-parser behavior).

use tree_sitter::{Node, Parser};

/// Commands beyond this size skip parsing entirely: tree-sitter has no
/// cancellation, so bound the pathological-input cost instead.
const MAX_PARSE_BYTES: usize = 64 * 1024;

/// Extract every command's text from a bash command string.
///
/// `Some(segments)` holds one entry per command in evaluation order —
/// compound lists, pipelines, and substitutions included, nesting
/// flattened. `None` means "cannot trust the parse"; the caller must
/// fall back to string segmentation.
pub fn parsed_command_segments(command: &str) -> Option<Vec<String>> {
    if command.len() > MAX_PARSE_BYTES {
        return None;
    }
    let source = command.as_bytes();
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(command, None)?;
    if tree.root_node().has_error() {
        return None;
    }
    let mut segments = Vec::new();
    collect_command_nodes(&tree.root_node(), source, &mut segments);
    Some(segments)
}

/// Walk the tree collecting command texts in source order.
///
/// Substitution nests inside a command's words (`echo $(curl evil)`),
/// so the walk continues below a collected command node to surface the
/// inner commands too — the outer text matches as-is (conservative for
/// deny), the inner text keeps `curl *`-style rules effective. Each
/// command node contributes its full text and, when different, its bare
/// assignment-stripped text.
///
/// Heredoc bodies get special treatment: the grammar keeps plain body
/// lines as hidden tokens (no nodes of their own) inside a
/// `heredoc_body` container, yet `bash <<EOF` really executes them —
/// dropping the body would be a deny escape. Body text goes through
/// string segmentation (pre-parser coverage level); command
/// substitutions inside the body are also collected by the walk below,
/// so nothing is lost by not AST-parsing the content.
fn collect_command_nodes(node: &Node, source: &[u8], segments: &mut Vec<String>) {
    if node.kind() == "command" {
        let full = node
            .utf8_text(source)
            .ok()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string);
        let bare = bare_command_text(node, source);
        if let Some(text) = &full {
            segments.push(text.clone());
        }
        if let Some(text) = bare
            && full.as_ref() != Some(&text)
            && !segments.contains(&text)
        {
            segments.push(text);
        }
    }
    if node.kind() == "heredoc_body"
        && let Some(text) = node.utf8_text(source).ok()
    {
        segments.extend(
            crate::split_command_segments(text)
                .into_iter()
                .map(str::trim)
                .filter(|segment| !segment.is_empty())
                .map(str::to_string),
        );
    }
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            collect_command_nodes(&cursor.node(), source, segments);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

/// A command node's bare text: direct children in source order with
/// leading `variable_assignment` (and comments) dropped, joined by
/// single spaces — `X=1 curl   evil` -> `curl evil`. Returns `None`
/// when nothing beyond assignments remains.
fn bare_command_text(node: &Node, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return None;
    }
    let mut words: Vec<String> = Vec::new();
    loop {
        let child = cursor.node();
        if child.kind() != "variable_assignment"
            && child.kind() != "comment"
            && let Ok(text) = child.utf8_text(source)
            && !text.trim().is_empty()
        {
            words.push(text.trim().to_string());
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
    if words.is_empty() {
        None
    } else {
        Some(words.join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(command: &str) -> Vec<String> {
        parsed_command_segments(command).expect("command should parse")
    }

    #[test]
    fn simple_and_compound_commands_extract() {
        assert_eq!(parse_ok("curl http://evil"), vec!["curl http://evil"]);
        assert_eq!(
            parse_ok("cd /tmp && curl http://evil"),
            vec!["cd /tmp", "curl http://evil"]
        );
        assert_eq!(
            parse_ok("git status | head -3"),
            vec!["git status", "head -3"]
        );
        // Substitutions nest inside the outer command's words: the
        // outer text matches as-is and the inner command surfaces on
        // its own, so `curl *`-style deny rules stay effective.
        assert_eq!(
            parse_ok("echo $(curl evil)"),
            vec!["echo $(curl evil)", "curl evil"]
        );
        assert_eq!(
            parse_ok("diff <(curl evil) x"),
            vec!["diff <(curl evil) x", "curl evil"]
        );
    }

    #[test]
    fn quoted_separators_no_longer_fabricate_commands() {
        // The string splitter cuts inside the quotes here and produces
        // a phantom `curl evil" ` segment; the parser sees one command
        // whose text is the raw source slice (quotes included).
        assert_eq!(
            parse_ok("echo \"hello; curl evil\""),
            vec!["echo \"hello; curl evil\""]
        );
        assert_eq!(parse_ok("echo 'a && b'"), vec!["echo 'a && b'"]);
    }

    #[test]
    fn env_prefix_commands_surface_their_name() {
        // The string splitter kept the `X=1 ` prefix, so `curl *` never
        // matched; the bare segment drops the assignment while the full
        // segment stays information-preserving.
        assert_eq!(
            parse_ok("X=1 curl evil"),
            vec!["X=1 curl evil", "curl evil"]
        );
        assert_eq!(parse_ok("sudo curl evil"), vec!["sudo curl evil"]);
    }

    #[test]
    fn comments_and_blanks_hold_no_commands() {
        assert_eq!(parse_ok("# just a comment"), Vec::<String>::new());
        assert_eq!(parse_ok("   \n  "), Vec::<String>::new());
    }

    #[test]
    fn parse_failure_is_conservative() {
        // Unterminated quote: the parser reports an ERROR node.
        assert_eq!(parsed_command_segments("echo \"unterminated"), None);
        // Oversized input skips parsing outright.
        let huge = "a".repeat(MAX_PARSE_BYTES + 1);
        assert_eq!(parsed_command_segments(&huge), None);
    }

    #[test]
    fn heredoc_body_lines_surface_as_segments() {
        // `bash <<EOF` really executes the body lines, but the grammar
        // keeps them as opaque tokens — the old string splitter saw
        // `curl evil` and so must the parser-aware extractor.
        let segments = parse_ok("bash <<EOF\ncurl evil\nEOF");
        assert!(
            segments.iter().any(|s| s.starts_with("curl")),
            "{segments:?}"
        );
        // Substitutions inside the body were already collected by the
        // plain walk; the body-line pass must not lose them.
        let segments = parse_ok("bash <<EOF\necho $(curl evil)\nEOF");
        assert!(
            segments.iter().any(|s| s.starts_with("curl")),
            "{segments:?}"
        );
    }
}
