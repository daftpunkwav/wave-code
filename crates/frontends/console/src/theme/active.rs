//! The active theme: a palette plus paint helpers, globally readable.
//!
//! Installed once at startup via [`set`]; readers clone (themes are tiny).

use std::sync::RwLock;

use tui_engine::color::{Color, Style};

use super::colors::{Palette, Token, dark_palette, light_palette};

/// The resolved theme: base kind + palette. `Copy` so readers clone it
/// cheaply per render.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    dark: bool,
    palette: Palette,
}

impl Theme {
    /// The dark theme.
    pub fn dark() -> Self {
        Self {
            dark: true,
            palette: dark_palette(),
        }
    }

    /// The light theme.
    pub fn light() -> Self {
        Self {
            dark: false,
            palette: light_palette(),
        }
    }

    /// Build a theme over an explicit palette (custom-theme support).
    pub fn from_palette(dark: bool, palette: Palette) -> Self {
        Self { dark, palette }
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
            Color::rgb(0x2D, 0xD4, 0xBF)
        );
    }

    #[test]
    fn paint_uses_active_palette() {
        set(Theme::dark());
        assert_eq!(
            current().paint(Token::Primary, "x"),
            "\x1b[38;2;45;212;191mx\x1b[0m"
        );
    }
}
