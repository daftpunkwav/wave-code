//! Semantic color tokens and the dark/light palettes.

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
}

/// A resolved palette: one color per token.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub primary: Color,
    pub accent: Color,
    pub text: Color,
    pub text_strong: Color,
    pub text_dim: Color,
    pub text_muted: Color,
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
}

impl Palette {
    /// Look up the color for a token.
    pub fn get(&self, token: Token) -> Color {
        match token {
            Token::Primary => self.primary,
            Token::Accent => self.accent,
            Token::Text => self.text,
            Token::TextStrong => self.text_strong,
            Token::TextDim => self.text_dim,
            Token::TextMuted => self.text_muted,
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
        }
    }
}

/// The dark palette (hex values locked by test).
pub fn dark_palette() -> Palette {
    Palette {
        primary: Color::from_hex("#4FA8FF").unwrap(),
        accent: Color::from_hex("#5BC0BE").unwrap(),
        text: Color::from_hex("#E0E0E0").unwrap(),
        text_strong: Color::from_hex("#F5F5F5").unwrap(),
        text_dim: Color::from_hex("#888888").unwrap(),
        text_muted: Color::from_hex("#6B6B6B").unwrap(),
        border: Color::from_hex("#5A5A5A").unwrap(),
        border_focus: Color::from_hex("#E8A838").unwrap(),
        success: Color::from_hex("#4EC87E").unwrap(),
        warning: Color::from_hex("#E8A838").unwrap(),
        error: Color::from_hex("#E85454").unwrap(),
        diff_added: Color::from_hex("#4EC87E").unwrap(),
        diff_removed: Color::from_hex("#E85454").unwrap(),
        diff_added_strong: Color::from_hex("#7AD99B").unwrap(),
        diff_removed_strong: Color::from_hex("#F08585").unwrap(),
        diff_gutter: Color::from_hex("#6B6B6B").unwrap(),
        diff_meta: Color::from_hex("#888888").unwrap(),
        role_user: Color::from_hex("#FFCB6B").unwrap(),
        shell_mode: Color::from_hex("#BD93F9").unwrap(),
    }
}

/// The light palette (WCAG-AA tuned; hex values locked by test).
pub fn light_palette() -> Palette {
    Palette {
        primary: Color::from_hex("#1565C0").unwrap(),
        accent: Color::from_hex("#00838F").unwrap(),
        text: Color::from_hex("#1A1A1A").unwrap(),
        text_strong: Color::from_hex("#1A1A1A").unwrap(),
        text_dim: Color::from_hex("#454545").unwrap(),
        text_muted: Color::from_hex("#5F5F5F").unwrap(),
        border: Color::from_hex("#737373").unwrap(),
        border_focus: Color::from_hex("#92660A").unwrap(),
        success: Color::from_hex("#0E7A38").unwrap(),
        warning: Color::from_hex("#92660A").unwrap(),
        error: Color::from_hex("#B91C1C").unwrap(),
        diff_added: Color::from_hex("#0E7A38").unwrap(),
        diff_removed: Color::from_hex("#B91C1C").unwrap(),
        diff_added_strong: Color::from_hex("#0E7A38").unwrap(),
        diff_removed_strong: Color::from_hex("#B91C1C").unwrap(),
        diff_gutter: Color::from_hex("#737373").unwrap(),
        diff_meta: Color::from_hex("#5F5F5F").unwrap(),
        role_user: Color::from_hex("#9A4A00").unwrap(),
        shell_mode: Color::from_hex("#7C3AED").unwrap(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The palette hex values are part of the visual contract.
    #[test]
    fn dark_palette_values_are_locked() {
        let palette = dark_palette();
        assert_eq!(palette.primary, Color::rgb(0x4F, 0xA8, 0xFF));
        assert_eq!(palette.role_user, Color::rgb(0xFF, 0xCB, 0x6B));
        assert_eq!(palette.shell_mode, Color::rgb(0xBD, 0x93, 0xF9));
        assert_eq!(palette.border_focus, Color::rgb(0xE8, 0xA8, 0x38));
    }

    #[test]
    fn light_palette_values_are_locked() {
        let palette = light_palette();
        assert_eq!(palette.primary, Color::rgb(0x15, 0x65, 0xC0));
        assert_eq!(palette.role_user, Color::rgb(0x9A, 0x4A, 0x00));
        assert_eq!(palette.shell_mode, Color::rgb(0x7C, 0x3A, 0xED));
    }

    #[test]
    fn every_token_resolves() {
        let palette = dark_palette();
        for token in [
            Token::Primary,
            Token::Accent,
            Token::Text,
            Token::TextStrong,
            Token::TextDim,
            Token::TextMuted,
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
        ] {
            let _ = palette.get(token);
        }
    }
}
