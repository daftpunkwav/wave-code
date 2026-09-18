//! Syntax highlighting for code blocks via syntect.
//!
//! Implements the engine's [`SyntaxHighlighter`] seam so the markdown
//! renderer colors fenced code without gaining a highlighting
//! dependency itself. The syntax set merges the syntect defaults with
//! the two-face extras (TypeScript, TOML, ...); the theme set stays
//! syntect's defaults. One `HighlightLines` state machine runs per
//! code block. Oversized blocks and unknown languages fall back to
//! plain lines — highlighting is an enhancement, never a rendering
//! risk.

use std::sync::OnceLock;

use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme, ThemeSet};
#[cfg(test)]
use syntect::parsing::SyntaxReference;
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;
use tui_engine::color::{Color, Style};
use tui_engine::markdown::SyntaxHighlighter;

/// Blocks above this size render plain: highlighting cost during
/// streaming must never starve the 50 ms flush cadence.
const MAX_HIGHLIGHT_BYTES: usize = 30_000;

/// The process-wide syntax definitions (newline-normalized variants
/// pair with `LinesWithEndings` below).
fn syntax_set() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(two_face::syntax::extra_newlines)
}

/// The process-wide color themes.
fn theme_set() -> &'static ThemeSet {
    static SET: OnceLock<ThemeSet> = OnceLock::new();
    SET.get_or_init(ThemeSet::load_defaults)
}

/// The theme matching the active console palette.
fn theme_for(dark: bool) -> &'static Theme {
    let name = if dark {
        "base16-ocean.dark"
    } else {
        "base16-ocean.light"
    };
    &theme_set().themes[name]
}

/// A highlighter over the syntect default syntax set.
struct SyntectHighlighter {
    /// Picks the dark or light color theme.
    dark: bool,
}

/// Build a highlighter for the active console theme. Cheap: the syntax
/// and theme sets are process-wide statics.
pub fn highlighter() -> Box<dyn SyntaxHighlighter> {
    Box::new(SyntectHighlighter {
        dark: crate::theme::current().is_dark(),
    })
}

impl SyntaxHighlighter for SyntectHighlighter {
    fn highlight(&self, code: &str, lang: Option<&str>) -> Vec<String> {
        let syntax = match lang {
            Some(lang) => syntax_set().find_syntax_by_token(lang),
            None => None,
        };
        let Some(syntax) = syntax else {
            // Unknown or absent language: keep the terminal default
            // color instead of painting prose with the theme base.
            return plain(code);
        };
        if code.len() > MAX_HIGHLIGHT_BYTES {
            return plain(code);
        }
        let mut state = HighlightLines::new(syntax, theme_for(self.dark));
        let mut out = Vec::new();
        for line in LinesWithEndings::from(code) {
            let Ok(ranges) = state.highlight_line(line, syntax_set()) else {
                return plain(code);
            };
            out.push(render_line(&ranges));
        }
        out
    }
}

/// Unstyled lines (the plain-highlighter contract).
fn plain(code: &str) -> Vec<String> {
    let mut lines: Vec<String> = code
        .strip_suffix('\n')
        .unwrap_or(code)
        .lines()
        .map(|l| l.to_string())
        .collect();
    if code.is_empty() {
        lines.clear();
    }
    lines
}

/// Paint one physical line from its styled spans, dropping the
/// trailing newline so the renderer may indent and wrap.
fn render_line(ranges: &[(syntect::highlighting::Style, &str)]) -> String {
    let mut out = String::new();
    for (style, text) in ranges {
        let text = text.strip_suffix('\n').unwrap_or(text);
        let text = text.strip_suffix('\r').unwrap_or(text);
        if text.is_empty() {
            continue;
        }
        let mut paint = Style::new().fg(Color::rgb(
            style.foreground.r,
            style.foreground.g,
            style.foreground.b,
        ));
        if style.font_style.contains(FontStyle::BOLD) {
            paint = paint.bold();
        }
        if style.font_style.contains(FontStyle::ITALIC) {
            paint = paint.italic();
        }
        if style.font_style.contains(FontStyle::UNDERLINE) {
            paint = paint.underline();
        }
        out.push_str(&paint.paint(text));
    }
    out
}

/// True when the syntax set resolved a real grammar for the token
/// (probes stay on the static set, so this is cheap).
#[cfg(test)]
fn find_syntax(token: &str) -> Option<&'static SyntaxReference> {
    syntax_set().find_syntax_by_token(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_code_gets_truecolor_paint() {
        let highlighter = SyntectHighlighter { dark: true };
        let lines = highlighter.highlight("fn main() {}\n", Some("rs"));
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].contains("\x1b[38;2;"),
            "truecolor fg expected: {:?}",
            lines[0]
        );
        assert_eq!(
            tui_engine::width::strip_ansi(&lines[0]),
            "fn main() {}",
            "text survives the paint"
        );
    }

    #[test]
    fn unknown_language_stays_plain() {
        let highlighter = SyntectHighlighter { dark: true };
        let lines = highlighter.highlight("just text\n", Some("definitely-not-a-lang"));
        assert_eq!(lines, vec!["just text".to_string()]);
        assert!(
            !lines[0].contains('\x1b'),
            "no SGR on plain: {:?}",
            lines[0]
        );
    }

    #[test]
    fn missing_language_stays_plain() {
        let highlighter = SyntectHighlighter { dark: false };
        let lines = highlighter.highlight("a\nb", None);
        assert_eq!(lines, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn oversized_blocks_render_plain() {
        let highlighter = SyntectHighlighter { dark: true };
        let code = format!("fn big() {{}}\n{}", "let x = 1;\n".repeat(4000));
        assert!(code.len() > MAX_HIGHLIGHT_BYTES);
        let lines = highlighter.highlight(&code, Some("rust"));
        assert_eq!(lines.len(), 4001);
        assert!(
            !lines.iter().any(|l| l.contains('\x1b')),
            "oversized blocks must not paint: {:?}",
            lines[0]
        );
    }

    #[test]
    fn trailing_newline_yields_no_trailing_empty_line() {
        let highlighter = SyntectHighlighter { dark: true };
        let lines = highlighter.highlight("let a = 1;\nlet b = 2;\n", Some("rust"));
        assert_eq!(lines.len(), 2);
        assert!(!lines[1].ends_with('\n'));
    }

    #[test]
    fn the_default_set_knows_common_languages() {
        // Core defaults plus the two-face extras coding agents hit
        // most often.
        for token in ["rs", "ts", "tsx", "py", "sh", "json", "toml", "md"] {
            assert!(find_syntax(token).is_some(), "missing grammar: {token}");
        }
    }
}
