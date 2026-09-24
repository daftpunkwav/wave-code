//! The active theme: a palette plus paint helpers, globally readable.
//!
//! Installed once at startup via [`set`]; readers clone (themes are tiny).
//! A theme pairs a chrome palette with the syntax theme that colors
//! code blocks, so one selection drives both.

use std::sync::RwLock;

use tui_engine::color::{Color, Style};

use super::colors::{Palette, Token, deepwave_palette, light_palette, synthwave_palette};
use super::syntax::SyntaxTheme;

/// The resolved theme: base kind + palette + syntax theme. `Copy` so
/// readers clone it cheaply per render.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    dark: bool,
    palette: Palette,
    syntax_theme: SyntaxTheme,
}

impl Theme {
    /// The default theme: the synthwave identity.
    pub fn synthwave() -> Self {
        Self {
            dark: true,
            palette: synthwave_palette(),
            syntax_theme: SyntaxTheme::Synthwave84,
        }
    }

    /// The deepwave identity (kept selectable).
    pub fn deepwave() -> Self {
        Self {
            dark: true,
            palette: deepwave_palette(),
            syntax_theme: SyntaxTheme::OceanDark,
        }
    }

    /// The light theme.
    pub fn light() -> Self {
        Self {
            dark: false,
            palette: light_palette(),
            syntax_theme: SyntaxTheme::OceanLight,
        }
    }

    /// Build a theme over an explicit palette and syntax choice
    /// (custom-theme support).
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
    palette: Palette {
        primary: Color { r: 0, g: 0, b: 0 },
        accent: Color { r: 0, g: 0, b: 0 },
        text: Color { r: 0, g: 0, b: 0 },
        text_strong: Color { r: 0, g: 0, b: 0 },
        text_dim: Color { r: 0, g: 0, b: 0 },
        text_muted: Color { r: 0, g: 0, b: 0 },
        code_span: Color { r: 0, g: 0, b: 0 },
        border: Color { r: 0, g: 0, b: 0 },
        border_focus: Color { r: 0, g: 0, b: 0 },
        success: Color { r: 0, g: 0, b: 0 },
        warning: Color { r: 0, g: 0, b: 0 },
        error: Color { r: 0, g: 0, b: 0 },
        diff_added: Color { r: 0, g: 0, b: 0 },
        diff_removed: Color { r: 0, g: 0, b: 0 },
        diff_added_strong: Color { r: 0, g: 0, b: 0 },
        diff_removed_strong: Color { r: 0, g: 0, b: 0 },
        diff_gutter: Color { r: 0, g: 0, b: 0 },
        diff_meta: Color { r: 0, g: 0, b: 0 },
        role_user: Color { r: 0, g: 0, b: 0 },
        shell_mode: Color { r: 0, g: 0, b: 0 },
    },
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
        set(Theme::synthwave());
        assert!(current().is_dark());
        assert_eq!(
            current().color(Token::Primary),
            Color::rgb(0x36, 0xF9, 0xF6)
        );
    }

    #[test]
    fn paint_uses_active_palette() {
        set(Theme::synthwave());
        assert_eq!(
            current().paint(Token::Primary, "x"),
            "\x1b[38;2;54;249;246mx\x1b[0m"
        );
    }

    #[test]
    fn each_named_theme_carries_its_syntax_counterpart() {
        assert_eq!(Theme::synthwave().syntax_theme(), SyntaxTheme::Synthwave84);
        assert_eq!(Theme::deepwave().syntax_theme(), SyntaxTheme::OceanDark);
        assert_eq!(Theme::light().syntax_theme(), SyntaxTheme::OceanLight);
    }

    #[test]
    fn from_parts_composes_custom_themes() {
        let theme = Theme::from_parts(false, light_palette(), SyntaxTheme::Synthwave84);
        assert!(!theme.is_dark());
        assert_eq!(theme.syntax_theme(), SyntaxTheme::Synthwave84);
    }
}
