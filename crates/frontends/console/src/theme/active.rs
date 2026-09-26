//! The active theme: a palette plus paint helpers, globally readable.
//!
//! Installed once at startup via [`set`]; readers clone (themes are tiny).
//! A theme pairs a chrome palette with the syntax theme that colors
//! code blocks, so one selection drives both. The named constructors
//! resolve through the bundled theme data files (`builtin.rs`); no
//! color values live in this module.

use std::sync::RwLock;

use tui_engine::color::{Color, Style};

use super::builtin;
use super::syntax::SyntaxTheme;
use super::tokens::{Palette, Token};

/// The resolved theme: base kind + palette + syntax theme. `Copy` so
/// readers clone it cheaply per render.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    dark: bool,
    palette: Palette,
    syntax_theme: SyntaxTheme,
}

impl Theme {
    /// The default theme (the bundled `dark` identity).
    pub fn dark() -> Self {
        builtin::get("dark").expect("the bundled dark theme must resolve")
    }

    /// The `deepwave` identity (kept selectable).
    pub fn deepwave() -> Self {
        builtin::get("deepwave").expect("the bundled deepwave theme must resolve")
    }

    /// The light theme.
    pub fn light() -> Self {
        builtin::get("light").expect("the bundled light theme must resolve")
    }

    /// Build a theme over an explicit palette and syntax choice
    /// (resolved theme files compose through here).
    pub fn from_parts(dark: bool, palette: Palette, syntax_theme: SyntaxTheme) -> Self {
        Self {
            dark,
            palette,
            syntax_theme,
        }
    }

    /// True for the dark base.
    pub fn is_dark(&self) -> bool {
        self.dark
    }

    /// The color for a token.
    pub fn color(&self, token: Token) -> Color {
        self.palette.get(token)
    }

    /// An engine style for a token.
    pub fn style(&self, token: Token) -> Style {
        Style::new().fg(self.palette.get(token))
    }

    /// The syntax theme that colors code blocks.
    pub fn syntax_theme(&self) -> SyntaxTheme {
        self.syntax_theme
    }

    /// Paint text in a token color.
    pub fn paint(&self, token: Token, text: &str) -> String {
        self.style(token).paint(text)
    }

    /// Bold text in a token color.
    pub fn bold(&self, token: Token, text: &str) -> String {
        self.style(token).bold().paint(text)
    }

    /// Dim text in a token color.
    pub fn dim(&self, token: Token, text: &str) -> String {
        self.style(token).dim().paint(text)
    }

    /// Italic text in a token color.
    pub fn italic(&self, token: Token, text: &str) -> String {
        self.style(token).italic().paint(text)
    }

    /// The palette (custom themes compose over this).
    pub fn palette(&self) -> Palette {
        self.palette
    }
}

static ACTIVE: RwLock<Theme> = RwLock::new(Theme {
    dark: true,
    palette: Palette::black(),
    syntax_theme: SyntaxTheme::Synthwave84,
});

/// Install the active theme (call once at UI startup, before rendering).
pub fn set(theme: Theme) {
    *ACTIVE.write().expect("theme lock poisoned") = theme;
}

/// Read the active theme (clones: themes are tiny Copy structs).
pub fn current() -> Theme {
    *ACTIVE.read().expect("theme lock poisoned")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_defaults_and_set_roundtrips() {
        set(Theme::light());
        assert!(!current().is_dark());
        set(Theme::dark());
        assert!(current().is_dark());
        assert_eq!(
            current().color(Token::Primary),
            Color::rgb(0x4D, 0xA5, 0xFF)
        );
    }

    #[test]
    fn paint_uses_active_palette() {
        set(Theme::dark());
        assert_eq!(
            current().paint(Token::Primary, "x"),
            "\x1b[38;2;77;165;255mx\x1b[0m"
        );
    }

    #[test]
    fn each_named_theme_carries_its_syntax_counterpart() {
        assert_eq!(Theme::dark().syntax_theme(), SyntaxTheme::Synthwave84);
        assert_eq!(Theme::deepwave().syntax_theme(), SyntaxTheme::OceanDark);
        assert_eq!(Theme::light().syntax_theme(), SyntaxTheme::OceanLight);
    }

    #[test]
    fn from_parts_composes_resolved_files() {
        let theme = Theme::from_parts(false, Palette::black(), SyntaxTheme::Synthwave84);
        assert!(!theme.is_dark());
        assert_eq!(theme.syntax_theme(), SyntaxTheme::Synthwave84);
    }
}
