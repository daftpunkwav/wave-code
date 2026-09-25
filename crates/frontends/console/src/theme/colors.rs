//! Semantic color tokens and the palettes.
//!
//! The default dark palette is the **synthwave** identity: neon cyan,
//! periwinkle, and amber on deep purple-dark, retro-80s. The previous
//! identity, **deepwave / sonar**, stays selectable. Both pair with a
//! syntax theme (see `syntax.rs`): synthwave drives the bundled
//! SynthWave '84 tmTheme, deepwave rides base16-ocean.dark, and the
//! light palette rides base16-ocean.light.
//!
//! Degradation (see `tui_engine::color::ColorDepth`): truecolor is the
//! default; 256-color terminals get the nearest xterm index; the rest
//! fall back to the 16 classic ANSI colors, per role roughly —
//! Primary/RoleUser/BorderFocus → bright cyan, Accent/ShellMode →
//! bright blue/magenta, Success/DiffAdded → bright green,
//! Warning → bright red (salmon band), Error/DiffRemoved → bright red,
//! Text/TextStrong/TextDim/TextMuted/DiffMeta → white/silver,
//! Border/DiffGutter → blue/silver.

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
        }
    }
}

/// The dark palette: the **synthwave** identity (hex values locked by
/// test). Neon cyan leads the chrome, amber carries user input, and
/// the neon family maps onto the message categories:
///
/// | Token      | Role                          | Hue                    |
/// |------------|-------------------------------|------------------------|
/// | Primary    | assistant, prompt, headings   | cyan `#36F9F6`         |
/// | RoleUser   | user input                    | amber `#FFB86C`        |
/// | Accent     | secondary accents             | periwinkle `#B6B1FF`   |
/// | Success    | tool results, checks          | green `#72F1B8`        |
/// | Warning    | auto badges, warnings         | salmon `#F97E72`       |
/// | Error      | errors                        | red-pink `#FE4450`     |
/// | CodeSpan   | inline code                   | cyan `#36F9F6`         |
pub fn synthwave_palette() -> Palette {
    Palette {
        // Neon cyan: prompt, headings, focus chrome (legible on the
        // deep purple-dark; pink would fight the error/salmon band).
        primary: Color::from_hex("#36F9F6").unwrap(),
        // Secondary accents, queue markers, approval pointers.
        accent: Color::from_hex("#B6B1FF").unwrap(),
        text: Color::from_hex("#EDEAF0").unwrap(),
        text_strong: Color::from_hex("#FFFFFF").unwrap(),
        // Thinking text, hints, quotes.
        text_dim: Color::from_hex("#8B85A8").unwrap(),
        // Tips, link urls, code fences.
        text_muted: Color::from_hex("#6C5F9C").unwrap(),
        // Inline code spans ride the neon cyan.
        code_span: Color::from_hex("#36F9F6").unwrap(),
        border: Color::from_hex("#4A4166").unwrap(),
        border_focus: Color::from_hex("#36F9F6").unwrap(),
        success: Color::from_hex("#72F1B8").unwrap(),
        warning: Color::from_hex("#F97E72").unwrap(),
        error: Color::from_hex("#FE4450").unwrap(),
        diff_added: Color::from_hex("#72F1B8").unwrap(),
        diff_removed: Color::from_hex("#FE4450").unwrap(),
        diff_added_strong: Color::from_hex("#97F1D8").unwrap(),
        diff_removed_strong: Color::from_hex("#FF5E5B").unwrap(),
        diff_gutter: Color::from_hex("#4A4166").unwrap(),
        diff_meta: Color::from_hex("#8B85A8").unwrap(),
        // User input: warm amber — one readable accent against the
        // neutral agent speech, distinct from the salmon error band.
        role_user: Color::from_hex("#FFB86C").unwrap(),
        shell_mode: Color::from_hex("#B084EB").unwrap(),
        // Lines and input chrome: a true gray, no purple cast.
        neutral: Color::from_hex("#C9C6D2").unwrap(),
        // The user input band: a few steps lighter than the background.
        input_bg: Color::from_hex("#322C42").unwrap(),
    }
}

/// The previous dark palette: the **deepwave / sonar** identity (hex
/// values locked by test), kept as a selectable theme. An ocean
/// oscilloscope spectrum on deep slate, riding base16-ocean.dark for
/// syntax. Teal leads; the waveform categories span the ocean
/// spectrum.
pub fn deepwave_palette() -> Palette {
    Palette {
        // Sine wave / assistant speech, prompt, headings, focus chrome.
        primary: Color::from_hex("#2DD4BF").unwrap(),
        // Tool cards, queue markers, approval pointers (azure band).
        accent: Color::from_hex("#60A5FA").unwrap(),
        text: Color::from_hex("#D8E1E8").unwrap(),
        text_strong: Color::from_hex("#F0F6F9").unwrap(),
        // Triangle wave / thinking, hints, quotes.
        text_dim: Color::from_hex("#8B9BB4").unwrap(),
        text_muted: Color::from_hex("#5E6C82").unwrap(),
        // Inline code: warm sand against the teal chrome.
        code_span: Color::from_hex("#D8B871").unwrap(),
        border: Color::from_hex("#3A5550").unwrap(),
        border_focus: Color::from_hex("#5EEAD4").unwrap(),
        // Neutral result marks / tool results (sea-green band).
        success: Color::from_hex("#5FB878").unwrap(),
        warning: Color::from_hex("#E5C07B").unwrap(),
        error: Color::from_hex("#E06C75").unwrap(),
        diff_added: Color::from_hex("#5FB878").unwrap(),
        diff_removed: Color::from_hex("#E06C75").unwrap(),
        diff_added_strong: Color::from_hex("#7FD79A").unwrap(),
        diff_removed_strong: Color::from_hex("#F0939A").unwrap(),
        diff_gutter: Color::from_hex("#4A5866").unwrap(),
        diff_meta: Color::from_hex("#8B9BB4").unwrap(),
        // Square wave / user input (cyan band).
        role_user: Color::from_hex("#22D3EE").unwrap(),
        shell_mode: Color::from_hex("#BD93F9").unwrap(),
        neutral: Color::from_hex("#BEC3C7").unwrap(),
        input_bg: Color::from_hex("#24303A").unwrap(),
    }
}

/// The light palette (WCAG-AA tuned; hex values locked by test). The
/// deepwave roles shift to their dark-on-light teal/cyan equivalents.
pub fn light_palette() -> Palette {
    Palette {
        primary: Color::from_hex("#0F766E").unwrap(),
        accent: Color::from_hex("#0369A1").unwrap(),
        text: Color::from_hex("#1A2B32").unwrap(),
        text_strong: Color::from_hex("#0F1A1E").unwrap(),
        text_dim: Color::from_hex("#44586B").unwrap(),
        text_muted: Color::from_hex("#5F7486").unwrap(),
        code_span: Color::from_hex("#7A5C10").unwrap(),
        border: Color::from_hex("#8FA6A2").unwrap(),
        border_focus: Color::from_hex("#0F766E").unwrap(),
        success: Color::from_hex("#0E7A38").unwrap(),
        warning: Color::from_hex("#92660A").unwrap(),
        error: Color::from_hex("#B91C1C").unwrap(),
        diff_added: Color::from_hex("#0E7A38").unwrap(),
        diff_removed: Color::from_hex("#B91C1C").unwrap(),
        diff_added_strong: Color::from_hex("#0E7A38").unwrap(),
        diff_removed_strong: Color::from_hex("#B91C1C").unwrap(),
        diff_gutter: Color::from_hex("#737373").unwrap(),
        diff_meta: Color::from_hex("#5F5F5F").unwrap(),
        role_user: Color::from_hex("#155E75").unwrap(),
        shell_mode: Color::from_hex("#7C3AED").unwrap(),
        neutral: Color::from_hex("#5F6B73").unwrap(),
        input_bg: Color::from_hex("#E3EAEE").unwrap(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The synthwave hex values are part of the visual contract.
    #[test]
    fn synthwave_palette_values_are_locked() {
        let palette = synthwave_palette();
        assert_eq!(palette.primary, Color::rgb(0x36, 0xF9, 0xF6));
        assert_eq!(palette.role_user, Color::rgb(0xFF, 0xB8, 0x6C));
        assert_eq!(palette.accent, Color::rgb(0xB6, 0xB1, 0xFF));
        assert_eq!(palette.text, Color::rgb(0xED, 0xEA, 0xF0));
        assert_eq!(palette.text_dim, Color::rgb(0x8B, 0x85, 0xA8));
        assert_eq!(palette.text_muted, Color::rgb(0x6C, 0x5F, 0x9C));
        assert_eq!(palette.code_span, Color::rgb(0x36, 0xF9, 0xF6));
        assert_eq!(palette.border, Color::rgb(0x4A, 0x41, 0x66));
        assert_eq!(palette.border_focus, Color::rgb(0x36, 0xF9, 0xF6));
        assert_eq!(palette.success, Color::rgb(0x72, 0xF1, 0xB8));
        assert_eq!(palette.warning, Color::rgb(0xF9, 0x7E, 0x72));
        assert_eq!(palette.error, Color::rgb(0xFE, 0x44, 0x50));
        assert_eq!(palette.shell_mode, Color::rgb(0xB0, 0x84, 0xEB));
        assert_eq!(palette.neutral, Color::rgb(0xC9, 0xC6, 0xD2));
        assert_eq!(palette.input_bg, Color::rgb(0x32, 0x2C, 0x42));
    }

    /// The neutral line gray must carry no accent hue: channels stay
    /// within a narrow band of each other.
    #[test]
    fn neutral_is_a_true_gray() {
        for palette in [synthwave_palette(), deepwave_palette()] {
            let n = palette.neutral;
            let (mn, mx) = (n.r.min(n.g).min(n.b), n.r.max(n.g).max(n.b));
            assert!(
                mx as i32 - mn as i32 <= 12,
                "neutral must be desaturated: {n:?}"
            );
        }
    }

    /// User amber sits far from the salmon warning band: the two must
    /// stay distinguishable at a glance.
    #[test]
    fn role_user_amber_keeps_distance_from_warning_salmon() {
        let palette = synthwave_palette();
        let distance = |a: Color, b: Color| {
            let dr = a.r as i32 - b.r as i32;
            let dg = a.g as i32 - b.g as i32;
            let db = a.b as i32 - b.b as i32;
            (dr * dr + dg * dg + db * db) as f32
        };
        let warning = palette.warning;
        assert!(
            distance(palette.role_user, warning) > 3_000.0,
            "amber must not collapse into the salmon band: {palette:?}"
        );
        // Amber reads warm: red dominates blue by a wide margin.
        assert!(
            palette.role_user.r as i32 - palette.role_user.b as i32 > 100,
            "user amber is a warm hue: {palette:?}"
        );
    }

    /// The synthwave identity: neon cyan leads, backgrounds stay deep.
    #[test]
    fn synthwave_is_the_neon_identity() {
        let palette = synthwave_palette();
        // Primary cyan: green and blue dominate red.
        assert!(palette.primary.g > palette.primary.r);
        assert!(palette.primary.b > palette.primary.r);
        // Error reads red: red channel dominates blue.
        assert!(palette.error.r > palette.error.b);
        // Text stays near-white for body copy.
        assert!(palette.text.r > 200 && palette.text.g > 200);
    }

    /// Palette restraint: plain body text never shares an accent hue
    /// and stays high-contrast against the background — near-white on
    /// dark themes, dark ink on the light one — so prose stays calm
    /// while accent colors keep to the chrome (prompt, headings,
    /// borders, diff, status).
    #[test]
    fn body_text_is_plain_and_never_an_accent_hue() {
        for palette in [synthwave_palette(), deepwave_palette(), light_palette()] {
            let text = palette.text;
            for accent in [
                palette.primary,
                palette.accent,
                palette.code_span,
                palette.success,
                palette.warning,
                palette.error,
                palette.role_user,
                palette.shell_mode,
                palette.diff_added,
                palette.diff_removed,
            ] {
                assert_ne!(text, accent, "text must not borrow an accent hue");
            }
        }
        // Dark themes: near-white body copy.
        for palette in [synthwave_palette(), deepwave_palette()] {
            let text = palette.text;
            assert!(
                text.r > 150 && text.g > 150 && text.b > 150,
                "dark-theme text stays near-white: {text:?}"
            );
        }
        // Light theme: dark ink.
        let light = light_palette().text;
        assert!(
            light.r < 100 && light.g < 100 && light.b < 100,
            "light-theme text stays dark: {light:?}"
        );
    }

    /// The deepwave hex values are part of the visual contract.
    #[test]
    fn deepwave_palette_values_are_locked() {
        let palette = deepwave_palette();
        assert_eq!(palette.primary, Color::rgb(0x2D, 0xD4, 0xBF));
        assert_eq!(palette.role_user, Color::rgb(0x22, 0xD3, 0xEE));
        assert_eq!(palette.accent, Color::rgb(0x60, 0xA5, 0xFA));
        assert_eq!(palette.code_span, Color::rgb(0xD8, 0xB8, 0x71));
        assert_eq!(palette.shell_mode, Color::rgb(0xBD, 0x93, 0xF9));
        assert_eq!(palette.border_focus, Color::rgb(0x5E, 0xEA, 0xD4));
    }

    /// The deepwave identity: teal leads, orange nowhere.
    #[test]
    fn deepwave_palette_has_no_orange_primary() {
        let palette = deepwave_palette();
        // Teal channel order: green >= red and blue high.
        assert!(palette.primary.g > palette.primary.r);
        assert!(palette.primary.b > palette.primary.r * 2);
        // Warm hues survive only as warning amber and code sand.
        for warm in [palette.warning, palette.code_span] {
            assert!(warm.r > warm.b, "warm roles stay warm");
        }
    }

    #[test]
    fn light_palette_values_are_locked() {
        let palette = light_palette();
        assert_eq!(palette.primary, Color::rgb(0x0F, 0x76, 0x6E));
        assert_eq!(palette.role_user, Color::rgb(0x15, 0x5E, 0x75));
        assert_eq!(palette.shell_mode, Color::rgb(0x7C, 0x3A, 0xED));
    }

    #[test]
    fn every_token_resolves() {
        for palette in [synthwave_palette(), deepwave_palette(), light_palette()] {
            for token in [
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
            ] {
                let _ = palette.get(token);
            }
        }
    }
}
