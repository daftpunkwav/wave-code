//! Syntax highlighting for code blocks via syntect.
//!
//! Implements the engine's [`SyntaxHighlighter`] seam so the markdown
//! renderer colors fenced code without gaining a highlighting
//! dependency itself. The syntax set is the two-face extras; the theme
//! set adds the bundled SynthWave '84 tmTheme
//! (`assets/synthwave-84.tmTheme`) on top of the registered defaults,
//! and which theme colors a block is decided by the active console
//! theme's [`SyntaxTheme`], so selecting a chrome theme selects its
//! code palette too. One `HighlightLines` state machine runs per code
//! block. Oversized blocks and unknown languages fall back to plain
//! lines — highlighting is an enhancement, never a rendering risk.
//!
//! Backgrounds never render: only each span's foreground and font
//! style are read, so code always sits on the terminal's own
//! background color.

use std::sync::OnceLock;

use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme, ThemeSet};
#[cfg(test)]
use syntect::parsing::SyntaxReference;
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;
use tui_engine::color::{Color, Style};
use tui_engine::markdown::SyntaxHighlighter;

use crate::theme::syntax::SyntaxTheme;

/// The SynthWave '84 tmTheme bundled into the binary (derived from the
/// robb0wen/synthwave-vscode theme, MIT).
const SYNTHWAVE_84_TMTHEME: &str = include_str!("../assets/synthwave-84.tmTheme");

/// Blocks above this size render plain: highlighting cost during
/// streaming must never starve the 50 ms flush cadence.
const MAX_HIGHLIGHT_BYTES: usize = 30_000;

/// The process-wide syntax definitions (newline-normalized variants
/// pair with `LinesWithEndings` below).
fn syntax_set() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(two_face::syntax::extra_newlines)
}

/// The process-wide color themes: the two-face bundle (which includes
/// syntect's defaults) plus the bundled synthwave-84 theme.
fn theme_set() -> &'static ThemeSet {
    static SET: OnceLock<ThemeSet> = OnceLock::new();
    SET.get_or_init(|| {
        let mut set: ThemeSet = two_face::theme::extra().into();
        let synthwave = ThemeSet::load_from_reader(&mut std::io::Cursor::new(SYNTHWAVE_84_TMTHEME))
            .expect("bundled synthwave-84 tmTheme must parse");
        set.themes
            .insert(SyntaxTheme::Synthwave84.name().to_string(), synthwave);
        set
    })
}

/// The theme selected by the syntax-theme choice.
fn theme_for(syntax: SyntaxTheme) -> &'static Theme {
    let name = syntax.name();
    &theme_set().themes[name]
}

/// A highlighter over the syntect default syntax set.
struct SyntectHighlighter {
    /// The registered syntax theme painting the code.
    syntax_theme: SyntaxTheme,
}

/// Build a highlighter for the active console theme. Cheap: the syntax
/// and theme sets are process-wide statics.
pub fn highlighter() -> Box<dyn SyntaxHighlighter> {
    Box::new(SyntectHighlighter {
        syntax_theme: crate::theme::current().syntax_theme(),
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
        let mut state = HighlightLines::new(syntax, theme_for(self.syntax_theme));
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
/// trailing newline so the renderer may indent and wrap. Backgrounds
/// are deliberately dropped: the terminal owns the background.
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
        let highlighter = SyntectHighlighter {
            syntax_theme: SyntaxTheme::Synthwave84,
        };
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
        let highlighter = SyntectHighlighter {
            syntax_theme: SyntaxTheme::Synthwave84,
        };
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
        let highlighter = SyntectHighlighter {
            syntax_theme: SyntaxTheme::OceanLight,
        };
        let lines = highlighter.highlight("a\nb", None);
        assert_eq!(lines, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn oversized_blocks_render_plain() {
        let highlighter = SyntectHighlighter {
            syntax_theme: SyntaxTheme::Synthwave84,
        };
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
        let highlighter = SyntectHighlighter {
            syntax_theme: SyntaxTheme::Synthwave84,
        };
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

    #[test]
    fn every_syntax_theme_resolves_in_the_set() {
        // Registry names must match the theme set, or a selection
        // would panic at first use.
        for theme in [
            SyntaxTheme::Synthwave84,
            SyntaxTheme::OceanDark,
            SyntaxTheme::OceanLight,
        ] {
            assert!(
                theme_set().themes.contains_key(theme.name()),
                "missing theme: {}",
                theme.name()
            );
        }
    }

    /// The synthwave identity: editor-grade token separation — keyword
    /// (yellow), string (orange), comment (periwinkle italic), function
    /// (cyan), type (red-pink) must all carry distinct colors.
    #[test]
    fn synthwave_paints_distinct_token_families() {
        let code = "// note\nfn greet(name: &str) -> usize {\n    let count = 42;\n    if count > 0 { println!(\"hi\"); }\n    count\n}\n";
        let highlighter = SyntectHighlighter {
            syntax_theme: SyntaxTheme::Synthwave84,
        };
        let lines = highlighter.highlight(code, Some("rs"));
        let joined = lines.join("\n");
        // Keyword `fn`/`let`/`if` in yellow.
        assert!(
            joined.contains("\x1b[38;2;254;222;93m"),
            "keyword yellow: {joined:?}"
        );
        // String "hi" in orange.
        assert!(
            joined.contains("\x1b[38;2;255;139;57m"),
            "string orange: {joined:?}"
        );
        // Comment in periwinkle italic (SGR 3 = italic).
        assert!(
            joined.contains("\x1b[38;2;132;139;189;3m"),
            "comment periwinkle italic: {joined:?}"
        );
        // Function name `greet` in cyan.
        assert!(
            joined.contains("\x1b[38;2;54;249;246m"),
            "function cyan: {joined:?}"
        );
        // Number 42 in salmon.
        assert!(
            joined.contains("\x1b[38;2;249;126;114m"),
            "number salmon: {joined:?}"
        );
    }

    #[test]
    fn no_background_sequences_are_emitted() {
        // 48;2 (background truecolor) or 48;5 must never appear: the
        // terminal's own background shows through code blocks.
        let code = "fn a() {}\n# hash\n\"str\"\n";
        let highlighter = SyntectHighlighter {
            syntax_theme: SyntaxTheme::Synthwave84,
        };
        let lines = highlighter.highlight(code, Some("rs"));
        let joined = lines.join("\n");
        assert!(
            !joined.contains("\x1b[48;"),
            "background SGR leaked: {joined:?}"
        );
    }

    /// Deepwave rides base16-ocean.dark: teal-adjacent keywords, a
    /// different look from synthwave on the same source.
    #[test]
    fn deepwave_selects_the_ocean_dark_palette() {
        let highlighter = SyntectHighlighter {
            syntax_theme: SyntaxTheme::OceanDark,
        };
        let lines = highlighter.highlight("fn main() {}\n", Some("rs"));
        let joined = lines.join("\n");
        assert!(joined.contains('\x1b'), "ocean dark paints: {joined:?}");
        // base16-ocean.dark keyword color #8FA1B3.
        assert!(
            joined.contains("\x1b[38;2;143;161;179m"),
            "ocean keyword: {joined:?}"
        );
    }
}
