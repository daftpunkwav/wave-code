//! Theme resolution: config choice → environment → terminal probe.

use std::io::IsTerminal as _;

use tui_engine::color::ColorDepth;
use tui_engine::terminal::{self, Background};

use super::active::Theme;

/// Resolve the theme for a `light` / `dark` / `deepwave` / `auto`
/// config choice (any other value means auto).
///
/// Auto detection: non-terminal or `NO_COLOR`/`FORCE_COLOR=0`/CI
/// environments skip probing (the synthwave default); otherwise the
/// OSC 11 background query runs under a short timeout, falling back to
/// `COLORFGBG` and finally the default. Call after entering raw mode
/// so the probe can read the reply.
pub fn resolve(choice: Option<&str>) -> Theme {
    match choice {
        Some("light") => return Theme::light(),
        Some("dark") => return Theme::synthwave(),
        Some("deepwave") => return Theme::deepwave(),
        _ => {}
    }
    if std::env::var_os("NO_COLOR").is_some()
        || std::env::var_os("CI").is_some()
        || std::env::var("FORCE_COLOR").is_ok_and(|v| v == "0")
    {
        return Theme::synthwave();
    }
    if !std::io::stdout().is_terminal() {
        return Theme::synthwave();
    }
    if let Some(background) = terminal::query_background(250) {
        return background_theme(background);
    }
    if let Ok(value) = std::env::var("COLORFGBG")
        && let Some(background) = terminal::background_from_colorfgbg(&value)
    {
        return background_theme(background);
    }
    Theme::synthwave()
}

/// Resolve the terminal color depth from the environment: `COLORTERM`
/// advertises truecolor, `TERM` advertises 256-color or direct color,
/// and Windows 10+ terminals speak truecolor natively even when `TERM`
/// is unset. Anything else degrades to the 16 classic ANSI colors
/// (conservative but always readable). Call once at startup.
pub fn color_depth() -> ColorDepth {
    color_depth_from(
        std::env::var("COLORTERM").ok().as_deref(),
        std::env::var("TERM").ok().as_deref(),
        cfg!(windows),
    )
}

/// Pure core of [`color_depth`], taking the environment values and a
/// Windows flag (Windows terminal hosts advertise truecolor without
/// setting the Unix environment variables).
fn color_depth_from(colorterm: Option<&str>, term: Option<&str>, windows: bool) -> ColorDepth {
    if colorterm.is_some_and(|v| v.contains("truecolor") || v.contains("24bit")) {
        return ColorDepth::TrueColor;
    }
    let Some(term) = term else {
        return if windows {
            ColorDepth::TrueColor
        } else {
            ColorDepth::Ansi16
        };
    };
    if term.contains("256color") {
        ColorDepth::Color256
    } else if term.contains("truecolor") || term.contains("direct") || windows {
        ColorDepth::TrueColor
    } else {
        ColorDepth::Ansi16
    }
}

fn background_theme(background: Background) -> Theme {
    match background {
        Background::Dark => Theme::synthwave(),
        Background::Light => Theme::light(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::syntax::SyntaxTheme;
    use super::*;

    #[test]
    fn explicit_choice_wins_over_environment() {
        // Explicit choices short-circuit before any probing, so they
        // hold in every environment (CI, NO_COLOR, non-TTY).
        assert!(!resolve(Some("light")).is_dark());
        assert!(resolve(Some("dark")).is_dark());
        assert!(resolve(Some("deepwave")).is_dark());
    }

    #[test]
    fn dark_choice_selects_the_default_synthwave_identity() {
        assert_eq!(
            resolve(Some("dark")).syntax_theme(),
            SyntaxTheme::Synthwave84
        );
        assert_eq!(
            resolve(Some("deepwave")).syntax_theme(),
            SyntaxTheme::OceanDark
        );
    }

    #[test]
    fn color_depth_follows_terminal_advertisement() {
        use ColorDepth::{Ansi16, Color256, TrueColor};
        let f = super::color_depth_from;
        assert_eq!(f(Some("truecolor"), None, false), TrueColor);
        assert_eq!(f(Some("24bit"), Some("xterm"), false), TrueColor);
        assert_eq!(f(None, Some("xterm-256color"), false), Color256);
        assert_eq!(f(None, Some("xterm-direct"), false), TrueColor);
        assert_eq!(f(None, Some("xterm"), false), Ansi16);
        assert_eq!(f(None, None, false), Ansi16);
        // Windows hosts speak truecolor without the Unix env vars.
        assert_eq!(f(None, None, true), TrueColor);
        assert_eq!(f(None, Some("xterm"), true), TrueColor);
        // 256color in TERM does not beat an explicit COLORTERM.
        assert_eq!(
            f(Some("truecolor"), Some("xterm-256color"), false),
            TrueColor
        );
    }
}
