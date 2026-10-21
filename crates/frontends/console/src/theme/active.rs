//! The active theme: a palette plus paint helpers, globally readable.
//!
//! Installed once at startup via [`set`]; readers clone (themes are tiny).
//! A theme pairs a chrome palette with the syntax theme that colors
//! code blocks, so one selection drives both. The named constructors
//! resolve through the bundled theme data files (`builtin.rs`); no
//! color values live in this module.
//!
//! The install is process-global in the shipped build. The test build
//! scopes it to the installing thread, because the harness runs the
//! crate's tests in parallel and a global theme would let one test's
//! palette leak into another test's assertions.

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

// The theme installed on this thread, in test builds.
//
// The test harness runs the crate's tests in parallel threads, and
// several of them install a theme other than the default (the light
// and deepwave palettes). A process-global theme makes those tests
// race: a test that asserts the dark palette can observe the light one
// while it runs, and fails for a reason that has nothing to do with
// what it checks.
//
// Scoping the installed theme to the thread that installed it removes
// the race without serializing the suite. The production build keeps
// the process global: the render loop and the theme install must see
// the same theme, and an async task may migrate between the runtime's
// workers.
#[cfg(test)]
thread_local! {
    static INSTALLED: std::cell::Cell<Option<Theme>> = const { std::cell::Cell::new(None) };
}

/// Install the active theme (call once at UI startup, before rendering).
#[cfg(not(test))]
pub fn set(theme: Theme) {
    *ACTIVE.write().expect("theme lock poisoned") = theme;
}

/// Install the active theme on the calling thread.
///
/// The test build scopes the install to the thread: the harness runs
/// the crate's tests in parallel, and a process-global theme would let
/// one test's palette leak into another test's assertions.
#[cfg(test)]
pub fn set(theme: Theme) {
    INSTALLED.with(|installed| installed.set(Some(theme)));
}

/// Read the active theme (clones: themes are tiny Copy structs).
#[cfg(not(test))]
pub fn current() -> Theme {
    *ACTIVE.read().expect("theme lock poisoned")
}

/// Read the active theme: the one installed on this thread, or the
/// default palette when this thread installed none.
#[cfg(test)]
pub fn current() -> Theme {
    INSTALLED
        .with(|installed| installed.get())
        .unwrap_or_else(|| *ACTIVE.read().expect("theme lock poisoned"))
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

    /// The installed theme is scoped to the thread that installed it.
    ///
    /// The test harness runs the crate's tests in parallel threads, and
    /// several of them install the light or the deepwave palette. With
    /// a process-global theme those tests race: a test that asserts the
    /// dark palette can observe the light one while it runs. Scoping
    /// the install to the thread removes the race.
    #[test]
    fn an_installed_theme_stays_on_its_own_thread() {
        set(Theme::light());
        assert!(!current().is_dark());
        let other = std::thread::spawn(|| current().is_dark())
            .join()
            .expect("the probe thread must not panic");
        assert!(other, "another thread keeps the default");
    }
}
