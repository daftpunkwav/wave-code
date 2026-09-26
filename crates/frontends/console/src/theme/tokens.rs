//! The semantic color contract: tokens and a resolved palette.
//!
//! This module carries NO color values. Values live in theme data
//! files (`themes/*.json` bundled, `~/.wavecode/themes/*.json`
//! user-authored); components request a token and the active palette
//! resolves it. See `file.rs` for the file format and `builtin.rs` for
//! the bundled themes.

use tui_engine::color::Color;

/// Semantic color roles used across the interface. Components never
/// hardcode hex values; they request a token from the active theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Token {
    /// Links, inline code, selected rows, focused chrome, spinners.
    Primary,
    /// Approval pointers, queue markers, secondary accents.
    Accent,
    /// Body text.
    Text,
    /// Emphasized dialog text.
    TextStrong,
    /// Thinking text, hints, cwd, quotes.
    TextDim,
    /// Tips, link urls, code fences.
    TextMuted,
    /// Inline code spans in rendered markdown.
    CodeSpan,
    /// Editor and pane borders.
    Border,
    /// The approval panel and other focus-critical chrome.
    BorderFocus,
    /// Success checks.
    Success,
    /// Auto/yolo badges and transient warnings.
    Warning,
    /// Errors.
    Error,
    /// Diff added lines.
    DiffAdded,
    /// Diff removed lines.
    DiffRemoved,
    /// Diff added counts (bold).
    DiffAddedStrong,
    /// Diff removed counts (bold).
    DiffRemovedStrong,
    /// Diff line-number gutters.
    DiffGutter,
    /// Diff metadata (elision rows).
    DiffMeta,
    /// User message bullet and text.
    RoleUser,
    /// Shell-mode prompt and border.
    ShellMode,
    /// Neutral lines and chrome: code frames, table borders, rules, the
    /// editor prompt/border — a true gray with no accent hue.
    Neutral,
    /// The user input row's subtle background highlight.
    InputBg,
    /// The terminal background the ink is tuned against (OSC 11 apply;
    /// never painted per-row).
    Background,
}

/// Every token the UI can request. A theme file may override any
/// subset; missing entries inherit from the file's `base` theme.
pub const ALL_TOKENS: [Token; 23] = [
    Token::Primary,
    Token::Accent,
    Token::Text,
    Token::TextStrong,
    Token::TextDim,
    Token::TextMuted,
    Token::CodeSpan,
    Token::Border,
    Token::BorderFocus,
    Token::Success,
    Token::Warning,
    Token::Error,
    Token::DiffAdded,
    Token::DiffRemoved,
    Token::DiffAddedStrong,
    Token::DiffRemovedStrong,
    Token::DiffGutter,
    Token::DiffMeta,
    Token::RoleUser,
    Token::ShellMode,
    Token::Neutral,
    Token::InputBg,
    Token::Background,
];

/// A resolved palette: one color per token.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub primary: Color,
    pub accent: Color,
    pub text: Color,
    pub text_strong: Color,
    pub text_dim: Color,
    pub text_muted: Color,
    /// Inline code spans in rendered markdown.
    pub code_span: Color,
    pub border: Color,
    pub border_focus: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
    pub diff_added: Color,
    pub diff_removed: Color,
    pub diff_added_strong: Color,
    pub diff_removed_strong: Color,
    pub diff_gutter: Color,
    pub diff_meta: Color,
    pub role_user: Color,
    pub shell_mode: Color,
    /// Neutral gray for lines and input chrome (no accent hue).
    pub neutral: Color,
    /// The user input row's background highlight.
    pub input_bg: Color,
    /// The terminal background the ink is tuned against.
    pub background: Color,
}

impl Palette {
    /// A palette with every slot black (the starting point for
    /// complete theme files, which must overwrite all 23 tokens).
    pub const fn black() -> Self {
        Self {
            primary: Color::rgb(0, 0, 0),
            accent: Color::rgb(0, 0, 0),
            text: Color::rgb(0, 0, 0),
            text_strong: Color::rgb(0, 0, 0),
            text_dim: Color::rgb(0, 0, 0),
            text_muted: Color::rgb(0, 0, 0),
            code_span: Color::rgb(0, 0, 0),
            border: Color::rgb(0, 0, 0),
            border_focus: Color::rgb(0, 0, 0),
            success: Color::rgb(0, 0, 0),
            warning: Color::rgb(0, 0, 0),
            error: Color::rgb(0, 0, 0),
            diff_added: Color::rgb(0, 0, 0),
            diff_removed: Color::rgb(0, 0, 0),
            diff_added_strong: Color::rgb(0, 0, 0),
            diff_removed_strong: Color::rgb(0, 0, 0),
            diff_gutter: Color::rgb(0, 0, 0),
            diff_meta: Color::rgb(0, 0, 0),
            role_user: Color::rgb(0, 0, 0),
            shell_mode: Color::rgb(0, 0, 0),
            neutral: Color::rgb(0, 0, 0),
            input_bg: Color::rgb(0, 0, 0),
            background: Color::rgb(0, 0, 0),
        }
    }

    /// Look up the color for a token.
    pub fn get(&self, token: Token) -> Color {
        match token {
            Token::Primary => self.primary,
            Token::Accent => self.accent,
            Token::Text => self.text,
            Token::TextStrong => self.text_strong,
            Token::TextDim => self.text_dim,
            Token::TextMuted => self.text_muted,
            Token::CodeSpan => self.code_span,
            Token::Border => self.border,
            Token::BorderFocus => self.border_focus,
            Token::Success => self.success,
            Token::Warning => self.warning,
            Token::Error => self.error,
            Token::DiffAdded => self.diff_added,
            Token::DiffRemoved => self.diff_removed,
            Token::DiffAddedStrong => self.diff_added_strong,
            Token::DiffRemovedStrong => self.diff_removed_strong,
            Token::DiffGutter => self.diff_gutter,
            Token::DiffMeta => self.diff_meta,
            Token::RoleUser => self.role_user,
            Token::ShellMode => self.shell_mode,
            Token::Neutral => self.neutral,
            Token::InputBg => self.input_bg,
            Token::Background => self.background,
        }
    }

    /// Overwrite the slot for `token` (theme-file resolution).
    pub fn set(&mut self, token: Token, color: Color) {
        match token {
            Token::Primary => self.primary = color,
            Token::Accent => self.accent = color,
            Token::Text => self.text = color,
            Token::TextStrong => self.text_strong = color,
            Token::TextDim => self.text_dim = color,
            Token::TextMuted => self.text_muted = color,
            Token::CodeSpan => self.code_span = color,
            Token::Border => self.border = color,
            Token::BorderFocus => self.border_focus = color,
            Token::Success => self.success = color,
            Token::Warning => self.warning = color,
            Token::Error => self.error = color,
            Token::DiffAdded => self.diff_added = color,
            Token::DiffRemoved => self.diff_removed = color,
            Token::DiffAddedStrong => self.diff_added_strong = color,
            Token::DiffRemovedStrong => self.diff_removed_strong = color,
            Token::DiffGutter => self.diff_gutter = color,
            Token::DiffMeta => self.diff_meta = color,
            Token::RoleUser => self.role_user = color,
            Token::ShellMode => self.shell_mode = color,
            Token::Neutral => self.neutral = color,
            Token::InputBg => self.input_bg = color,
            Token::Background => self.background = color,
        }
    }

    /// WCAG relative luminance of the palette's background.
    pub fn background_luminance(&self) -> f64 {
        luminance(self.background)
    }
}

/// WCAG relative luminance of an sRGB color.
pub fn luminance(color: Color) -> f64 {
    let channel = |v: u8| {
        let c = f64::from(v) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(color.r) + 0.7152 * channel(color.g) + 0.0722 * channel(color.b)
}

/// WCAG contrast ratio between two colors.
pub fn contrast(a: Color, b: Color) -> f64 {
    let (hi, lo) = (
        luminance(a).max(luminance(b)),
        luminance(a).min(luminance(b)),
    );
    (hi + 0.05) / (lo + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_token_slot_round_trips_through_set_and_get() {
        let mut palette = Palette {
            primary: Color::rgb(0, 0, 0),
            accent: Color::rgb(0, 0, 0),
            text: Color::rgb(0, 0, 0),
            text_strong: Color::rgb(0, 0, 0),
            text_dim: Color::rgb(0, 0, 0),
            text_muted: Color::rgb(0, 0, 0),
            code_span: Color::rgb(0, 0, 0),
            border: Color::rgb(0, 0, 0),
            border_focus: Color::rgb(0, 0, 0),
            success: Color::rgb(0, 0, 0),
            warning: Color::rgb(0, 0, 0),
            error: Color::rgb(0, 0, 0),
            diff_added: Color::rgb(0, 0, 0),
            diff_removed: Color::rgb(0, 0, 0),
            diff_added_strong: Color::rgb(0, 0, 0),
            diff_removed_strong: Color::rgb(0, 0, 0),
            diff_gutter: Color::rgb(0, 0, 0),
            diff_meta: Color::rgb(0, 0, 0),
            role_user: Color::rgb(0, 0, 0),
            shell_mode: Color::rgb(0, 0, 0),
            neutral: Color::rgb(0, 0, 0),
            input_bg: Color::rgb(0, 0, 0),
            background: Color::rgb(0, 0, 0),
        };
        for (index, token) in ALL_TOKENS.iter().enumerate() {
            let color = Color::rgb(index as u8 + 1, 0, 0);
            palette.set(*token, color);
            assert_eq!(palette.get(*token), color, "{token:?}");
        }
    }

    #[test]
    fn luminance_orders_black_gray_white() {
        let black_c = Color::rgb(0, 0, 0);
        let gray_c = Color::rgb(128, 128, 128);
        let white_c = Color::rgb(255, 255, 255);
        let black = luminance(black_c);
        let gray = luminance(gray_c);
        let white = luminance(white_c);
        assert!(black < gray && gray < white);
        // The ratio bounds WCAG uses.
        assert!((contrast(white_c, black_c) - 21.0).abs() < 0.1);
    }
}
