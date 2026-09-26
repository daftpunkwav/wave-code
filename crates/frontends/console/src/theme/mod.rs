//! Theme system: semantic tokens, theme data files, and resolution.
//!
//! Colors live in data, never in code: the bundled themes are
//! `themes/*.json` (see `builtin.rs`), user themes live in
//! `~/.wavecode/themes/*.json` (see `file.rs`), and every module here
//! owns one responsibility —
//!
//! - `tokens`: the semantic color contract (tokens + palette slots),
//! - `file`: the theme.json format, parsing, validation, storage,
//! - `builtin`: the bundled theme data files,
//! - `active`: the global active theme + paint helpers,
//! - `detect`: resolving the configured choice / terminal probing,
//! - `syntax`: the code-highlighting theme aliases.

pub mod active;
pub mod builtin;
pub mod detect;
pub mod file;
pub mod syntax;
pub mod tokens;

pub use active::{Theme, current, set};
pub use file::ThemeError;
pub use syntax::SyntaxTheme;
pub use tokens::{Palette, Token};

/// Sync the terminal's default colors with the active theme
/// (best-effort). The light theme applies its ink + paper (OSC 10/11)
/// and an accent cursor (OSC 12) so it stays readable and usable on
/// dark-terminal hosts — whose default near-white foreground and
/// cursor would otherwise vanish on the paper; dark themes reset the
/// terminal back to its own colors (OSC 110/111/112). Terminals that
/// ignore the sequences are unaffected, and non-terminal stdout
/// (tests, pipes) skips the write entirely.
pub fn apply_terminal_scheme() {
    if !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        return;
    }
    let theme = current();
    let scheme = (!theme.is_dark()).then(|| {
        let palette = theme.palette();
        (palette.text, palette.background, palette.primary)
    });
    tui_engine::terminal::set_color_scheme(scheme);
}
