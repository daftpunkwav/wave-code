//! The bundled themes: pure data files embedded at compile time.
//!
//! Each built-in is a complete `theme.json` (see `file.rs` for the
//! format) living in `themes/` next to this module — the same format a
//! user theme uses, so shipping a theme variant is authoring one JSON
//! file, not touching Rust. Parsed once on first use.

use std::sync::OnceLock;

use super::active::Theme;
use super::file::ThemeFile;
use super::syntax::SyntaxTheme;
use super::tokens::Palette;

const DARK_JSON: &str = include_str!("themes/dark.json");
const DEEPWAVE_JSON: &str = include_str!("themes/deepwave.json");
const LIGHT_JSON: &str = include_str!("themes/light.json");

/// The built-in ids, in picker order.
pub const IDS: [&str; 3] = ["dark", "deepwave", "light"];

struct BuiltinTheme {
    theme: Theme,
    description: Option<String>,
}

impl BuiltinTheme {
    fn parse(id: &'static str, json: &'static str) -> Self {
        let file = ThemeFile::parse(json)
            .unwrap_or_else(|error| panic!("bundled theme {id} must parse: {error}"));
        // Complete files never carry a base: they define everything.
        let theme = file
            .resolve(true)
            .unwrap_or_else(|error| panic!("bundled theme {id} must resolve: {error}"));
        Self {
            theme,
            description: file.description,
        }
    }
}

static BUILTINS: OnceLock<[BuiltinTheme; 3]> = OnceLock::new();

fn builtins() -> &'static [BuiltinTheme; 3] {
    BUILTINS.get_or_init(|| {
        [
            BuiltinTheme::parse("dark", DARK_JSON),
            BuiltinTheme::parse("deepwave", DEEPWAVE_JSON),
            BuiltinTheme::parse("light", LIGHT_JSON),
        ]
    })
}

fn index_of(id: &str) -> Option<usize> {
    IDS.iter().position(|known| *known == id)
}

/// The resolved built-in theme for `id` (`dark` / `deepwave` / `light`).
pub fn get(id: &str) -> Option<Theme> {
    let index = index_of(id)?;
    Some(builtins()[index].theme)
}

/// The built-in palette (the `base` user themes inherit from).
pub fn palette(id: &str) -> Option<Palette> {
    Some(get(id)?.palette())
}

/// The built-in's kind.
pub fn is_dark(id: &str) -> bool {
    get(id).is_some_and(|theme| theme.is_dark())
}

/// The built-in's syntax theme.
pub fn syntax(id: &str) -> Option<SyntaxTheme> {
    Some(get(id)?.syntax_theme())
}

/// The built-in's picker description.
pub fn description(id: &str) -> Option<&'static str> {
    let index = index_of(id)?;
    builtins()[index].description.as_deref()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::tokens::{ALL_TOKENS, Token, contrast};

    fn themes() -> Vec<Theme> {
        IDS.iter().map(|id| get(id).unwrap()).collect()
    }

    /// The visual contract: one accent hue carries the chrome, the
    /// semantic band is reused everywhere, nothing else is colored.
    #[test]
    fn each_theme_keeps_the_one_accent_structure() {
        for theme in themes() {
            let palette = theme.palette();
            assert_eq!(palette.role_user, palette.primary, "user rides the accent");
            assert_eq!(
                palette.code_span, palette.primary,
                "inline code rides the accent"
            );
            assert_eq!(
                palette.border_focus, palette.primary,
                "focus rides the accent"
            );
            assert_eq!(
                palette.shell_mode, palette.warning,
                "shell mode is caution amber"
            );
            assert_eq!(
                palette.diff_added, palette.success,
                "diff green is the semantic band"
            );
            assert_eq!(
                palette.diff_removed, palette.error,
                "diff red is the semantic band"
            );
            assert_ne!(palette.accent, palette.primary, "accent is a distinct step");
        }
        // Dark themes step accent lighter; light steps it darker.
        let dark = get("dark").unwrap().palette();
        assert!(dark.accent.r > dark.primary.r && dark.accent.g > dark.primary.g);
        let light = get("light").unwrap().palette();
        assert!(light.accent.r < light.primary.r && light.accent.b < light.primary.b);
    }

    /// The neutral line gray must carry no accent hue: channels stay
    /// within a narrow band of each other.
    #[test]
    fn neutral_is_a_true_gray() {
        for theme in themes() {
            let palette = theme.palette();
            let n = palette.neutral;
            let (mn, mx) = (n.r.min(n.g).min(n.b), n.r.max(n.g).max(n.b));
            assert!(
                mx as i32 - mn as i32 <= 12,
                "neutral must be desaturated: {n:?}"
            );
        }
    }

    /// No violet grays, anywhere in the neutral ramp. Violet is the
    /// signature of a lavender ramp: blue runs far past green while
    /// green hugs red (the original dark theme's grays read purple).
    /// A slate ramp with an even channel progression (deepwave) stays
    /// allowed, and green never drops below red (no warm drift).
    #[test]
    fn neutral_ramp_never_drifts_warm_or_violet() {
        for theme in themes() {
            let palette = theme.palette();
            for (name, color) in [
                ("text", palette.text),
                ("text_dim", palette.text_dim),
                ("text_muted", palette.text_muted),
                ("border", palette.border),
                ("neutral", palette.neutral),
                ("diff_gutter", palette.diff_gutter),
            ] {
                assert!(color.g >= color.r, "{name} must not drift warm: {color:?}");
                let violet = i32::from(color.b - color.g) - i32::from(color.g - color.r);
                assert!(
                    violet <= 12,
                    "{name} must not drift violet: {color:?} (violet {violet})"
                );
            }
        }
    }

    /// The readability contract: every text ramp step clears WCAG
    /// contrast against its own theme background — body ≥ 7:1, dim
    /// ≥ 4.5:1, muted ≥ 3.5:1 — so no theme ships unreadable text.
    #[test]
    fn text_ramp_is_contrast_locked_against_the_background() {
        for theme in themes() {
            let palette = theme.palette();
            let bg = palette.background;
            assert!(
                contrast(palette.text, bg) >= 7.0,
                "body text below 7:1: {palette:?}"
            );
            assert!(
                contrast(palette.text_dim, bg) >= 4.5,
                "dim text below 4.5:1: {palette:?}"
            );
            assert!(
                contrast(palette.text_muted, bg) >= 3.5,
                "muted text below 3.5:1: {palette:?}"
            );
        }
    }

    /// The background tracks the theme kind: deep for dark themes,
    /// paper for the light one (the OSC apply relies on this).
    #[test]
    fn background_matches_the_theme_kind() {
        for theme in themes() {
            let palette = theme.palette();
            if theme.is_dark() {
                assert!(
                    palette.background_luminance() < 0.1,
                    "dark background stays deep: {palette:?}"
                );
            } else {
                assert!(palette.background_luminance() > 0.8);
            }
        }
    }

    /// The input band reads as one visible but quiet step of the
    /// background: clearly apart from it (so the user row highlights)
    /// yet never a contrasting card.
    #[test]
    fn input_bg_is_a_subtle_background_step() {
        for theme in themes() {
            let palette = theme.palette();
            let delta = (palette.background_luminance()
                - crate::theme::tokens::luminance(palette.input_bg))
            .abs();
            assert!(
                (0.005..0.30).contains(&delta),
                "input band must stay quiet but visible: {palette:?} (delta {delta})"
            );
        }
    }

    /// The dark identity: azure leads, semantics are green, amber, and
    /// red, body text stays near-white.
    #[test]
    fn dark_theme_is_the_blue_identity() {
        let palette = get("dark").unwrap().palette();
        assert!(palette.primary.b > palette.primary.r);
        assert!(palette.primary.g > palette.primary.r);
        assert!(palette.error.r > palette.error.b);
        assert!(palette.text.r > 200 && palette.text.g > 200);
    }

    /// The deepwave identity: teal leads, warm hues survive only in
    /// the semantic band.
    #[test]
    fn deepwave_has_no_orange_primary() {
        let palette = get("deepwave").unwrap().palette();
        assert!(palette.primary.g > palette.primary.r);
        assert!(palette.primary.b > palette.primary.r * 2);
        assert!(palette.warning.r > palette.warning.b, "warning stays warm");
    }

    /// The light identity: dark ink on paper.
    #[test]
    fn light_theme_stays_dark_ink_on_paper() {
        let palette = get("light").unwrap().palette();
        assert!(
            palette.text.r < 100 && palette.text.g < 100 && palette.text.b < 100,
            "light-theme text stays dark: {:?}",
            palette.text
        );
        assert!(palette.background_luminance() > 0.8);
    }

    /// Palette restraint: plain body text never shares an accent hue.
    #[test]
    fn body_text_is_plain_and_never_an_accent_hue() {
        for theme in themes() {
            let palette = theme.palette();
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
                assert_ne!(palette.text, accent, "text must not borrow an accent hue");
            }
        }
    }

    /// The bundled files are complete: every token resolves in every
    /// theme, and each carries the kind + syntax pairing its identity
    /// promises.
    #[test]
    fn bundled_themes_resolve_every_token_with_their_syntax_pairing() {
        for theme in themes() {
            for token in ALL_TOKENS {
                let _ = theme.color(token);
            }
        }
        assert_eq!(
            get("dark").unwrap().syntax_theme(),
            SyntaxTheme::Synthwave84
        );
        assert_eq!(
            get("deepwave").unwrap().syntax_theme(),
            SyntaxTheme::OceanDark
        );
        assert_eq!(
            get("light").unwrap().syntax_theme(),
            SyntaxTheme::OceanLight
        );
    }

    /// The color data values are part of the visual contract; these
    /// locks guard accidental edits to the bundled JSON.
    #[test]
    fn dark_values_are_locked() {
        let palette = get("dark").unwrap().palette();
        assert_eq!(
            palette.get(Token::Primary),
            tui_engine::color::Color::rgb(0x4D, 0xA5, 0xFF)
        );
        assert_eq!(
            palette.get(Token::Accent),
            tui_engine::color::Color::rgb(0x8F, 0xC7, 0xFF)
        );
        assert_eq!(
            palette.get(Token::Text),
            tui_engine::color::Color::rgb(0xDE, 0xE2, 0xE7)
        );
        assert_eq!(
            palette.get(Token::TextDim),
            tui_engine::color::Color::rgb(0x99, 0xA2, 0xAC)
        );
        assert_eq!(
            palette.get(Token::TextMuted),
            tui_engine::color::Color::rgb(0x75, 0x80, 0x8B)
        );
        assert_eq!(
            palette.get(Token::Border),
            tui_engine::color::Color::rgb(0x39, 0x41, 0x4B)
        );
        assert_eq!(
            palette.get(Token::Success),
            tui_engine::color::Color::rgb(0x3F, 0xB9, 0x50)
        );
        assert_eq!(
            palette.get(Token::Warning),
            tui_engine::color::Color::rgb(0xD2, 0x99, 0x22)
        );
        assert_eq!(
            palette.get(Token::Error),
            tui_engine::color::Color::rgb(0xF8, 0x51, 0x49)
        );
        assert_eq!(
            palette.get(Token::Neutral),
            tui_engine::color::Color::rgb(0xA8, 0xAD, 0xB2)
        );
        assert_eq!(
            palette.get(Token::InputBg),
            tui_engine::color::Color::rgb(0x25, 0x2A, 0x32)
        );
        assert_eq!(
            palette.get(Token::Background),
            tui_engine::color::Color::rgb(0x1A, 0x1D, 0x23)
        );
    }

    /// The deepwave values are part of the visual contract.
    #[test]
    fn deepwave_values_are_locked() {
        let palette = get("deepwave").unwrap().palette();
        assert_eq!(
            palette.get(Token::Primary),
            tui_engine::color::Color::rgb(0x2D, 0xD4, 0xBF)
        );
        assert_eq!(
            palette.get(Token::Accent),
            tui_engine::color::Color::rgb(0x5E, 0xEA, 0xD4)
        );
        assert_eq!(
            palette.get(Token::Text),
            tui_engine::color::Color::rgb(0xD5, 0xDE, 0xE5)
        );
        assert_eq!(
            palette.get(Token::TextDim),
            tui_engine::color::Color::rgb(0x93, 0xA4, 0xB4)
        );
        assert_eq!(
            palette.get(Token::TextMuted),
            tui_engine::color::Color::rgb(0x7E, 0x8F, 0xA0)
        );
        assert_eq!(
            palette.get(Token::Border),
            tui_engine::color::Color::rgb(0x31, 0x42, 0x4A)
        );
        assert_eq!(
            palette.get(Token::Neutral),
            tui_engine::color::Color::rgb(0xB4, 0xBA, 0xC0)
        );
        assert_eq!(
            palette.get(Token::InputBg),
            tui_engine::color::Color::rgb(0x22, 0x30, 0x3A)
        );
        assert_eq!(
            palette.get(Token::Background),
            tui_engine::color::Color::rgb(0x18, 0x21, 0x26)
        );
    }

    /// The light values are part of the visual contract.
    #[test]
    fn light_values_are_locked() {
        let palette = get("light").unwrap().palette();
        assert_eq!(
            palette.get(Token::Primary),
            tui_engine::color::Color::rgb(0x09, 0x69, 0xDA)
        );
        assert_eq!(
            palette.get(Token::Accent),
            tui_engine::color::Color::rgb(0x05, 0x50, 0xAE)
        );
        assert_eq!(
            palette.get(Token::Text),
            tui_engine::color::Color::rgb(0x1F, 0x23, 0x28)
        );
        assert_eq!(
            palette.get(Token::TextDim),
            tui_engine::color::Color::rgb(0x5A, 0x64, 0x6D)
        );
        assert_eq!(
            palette.get(Token::TextMuted),
            tui_engine::color::Color::rgb(0x6E, 0x76, 0x81)
        );
        assert_eq!(
            palette.get(Token::Border),
            tui_engine::color::Color::rgb(0xD5, 0xDA, 0xE0)
        );
        assert_eq!(
            palette.get(Token::Neutral),
            tui_engine::color::Color::rgb(0x71, 0x76, 0x7B)
        );
        assert_eq!(
            palette.get(Token::InputBg),
            tui_engine::color::Color::rgb(0xDC, 0xE1, 0xE7)
        );
        assert_eq!(
            palette.get(Token::Background),
            tui_engine::color::Color::rgb(0xF6, 0xF8, 0xFA)
        );
    }
}
