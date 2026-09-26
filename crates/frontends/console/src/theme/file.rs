//! Theme files: one JSON document per theme.
//!
//! A theme is pure data. Built-in themes ship as bundled `themes/*.json`
//! files (see `builtin.rs`); user themes live in
//! `~/.wavecode/themes/<name>.json` and are picked by file stem, like
//! VS Code theme extensions.
//!
//! Format:
//!
//! ```json
//! {
//!   "dark": false,
//!   "base": "dark",
//!   "description": "my take on the ocean",
//!   "syntax_theme": "ocean-dark",
//!   "colors": { "primary": "#2DD4BF", "background": "#0E1A1F" }
//! }
//! ```
//!
//! - `colors` may override any subset of the 23 semantic tokens
//!   (`#rrggbb`); entries missing here inherit from `base`.
//! - `base` names a built-in theme to start from (default `dark`);
//!   built-in files are complete and never use it.
//! - `dark` tags the theme's kind (drives terminal color sync and
//!   picker behavior). It defaults to the `background` override's
//!   luminance when one is set, otherwise to the base's kind, then to
//!   `true`.
//! - `syntax_theme` selects the code-highlighting theme by alias.
//! - `description` is free text shown in the theme picker.
//!
//! Unknown fields, unknown color names, malformed colors, and unknown
//! syntax aliases are rejected: a typoed key must never silently
//! render as the base.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::active::Theme;
use super::builtin;
use super::syntax::SyntaxTheme;
use super::tokens::{ALL_TOKENS, Palette};

/// Themes root directory under the home directory.
pub fn themes_dir(home: &Path) -> PathBuf {
    home.join(".wavecode").join("themes")
}

/// One theme file: base palette plus per-token hex overrides, an
/// optional syntax-theme selection, and optional picker copy.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThemeFile {
    /// The theme's kind; see the module docs for the default chain.
    #[serde(default)]
    pub dark: Option<bool>,
    /// Built-in theme the overrides start from (default `dark`).
    #[serde(default)]
    pub base: Option<String>,
    /// Free text shown next to the theme name in the picker.
    #[serde(default)]
    pub description: Option<String>,
    /// Alias selecting the syntax highlighting theme; unknown aliases
    /// are rejected at load time.
    #[serde(default)]
    pub syntax_theme: Option<String>,
    /// Per-token hex overrides (`#rrggbb`).
    #[serde(default)]
    pub colors: ThemeColors,
}

/// Every overridable token, each optional. Unknown names are rejected
/// by serde: a typoed token must not silently render as the base.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThemeColors {
    pub primary: Option<String>,
    pub accent: Option<String>,
    pub text: Option<String>,
    pub text_strong: Option<String>,
    pub text_dim: Option<String>,
    pub text_muted: Option<String>,
    pub code_span: Option<String>,
    pub border: Option<String>,
    pub border_focus: Option<String>,
    pub success: Option<String>,
    pub warning: Option<String>,
    pub error: Option<String>,
    pub diff_added: Option<String>,
    pub diff_removed: Option<String>,
    pub diff_added_strong: Option<String>,
    pub diff_removed_strong: Option<String>,
    pub diff_gutter: Option<String>,
    pub diff_meta: Option<String>,
    pub role_user: Option<String>,
    pub shell_mode: Option<String>,
    pub neutral: Option<String>,
    pub input_bg: Option<String>,
    pub background: Option<String>,
}

/// Why a theme file could not be used.
#[derive(Debug)]
pub enum ThemeError {
    /// Filesystem failure.
    Io(std::io::Error),
    /// JSON parse failure.
    Json(serde_json::Error),
    /// A color override was not a valid `#rrggbb` hex string.
    Color(&'static str, String),
    /// A theme name failed the file-stem whitelist.
    Name(&'static str, String),
    /// A `syntax_theme` alias named no registered theme.
    SyntaxTheme(String),
    /// A `base` named no built-in theme.
    Base(String),
    /// A bundled (complete) theme left tokens or the kind undefined.
    Incomplete(&'static str),
}

impl std::fmt::Display for ThemeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ThemeError::Io(error) => write!(f, "theme IO failed: {error}"),
            ThemeError::Json(error) => write!(f, "theme JSON failed: {error}"),
            ThemeError::Color(name, value) => write!(f, "invalid color for {name}: {value}"),
            ThemeError::Name(what, value) => write!(f, "invalid theme {what}: {value}"),
            ThemeError::SyntaxTheme(value) => write!(f, "unknown syntax theme: {value}"),
            ThemeError::Base(value) => write!(f, "unknown base theme: {value}"),
            ThemeError::Incomplete(what) => {
                write!(f, "bundled theme is incomplete: missing {what}")
            }
        }
    }
}

impl std::error::Error for ThemeError {}

impl From<std::io::Error> for ThemeError {
    fn from(error: std::io::Error) -> Self {
        ThemeError::Io(error)
    }
}

impl From<serde_json::Error> for ThemeError {
    fn from(error: serde_json::Error) -> Self {
        ThemeError::Json(error)
    }
}

impl ThemeFile {
    /// Parse a theme document without resolving it.
    pub fn parse(text: &str) -> Result<Self, ThemeError> {
        Ok(serde_json::from_str(text)?)
    }

    /// Merge the color overrides onto `base`. Missing tokens keep the
    /// base values.
    pub fn palette(&self, base: Palette) -> Result<Palette, ThemeError> {
        let mut palette = base;
        for token in ALL_TOKENS {
            let name = token_name(token);
            if let Some(hex) = override_of(&self.colors, token) {
                let color = tui_engine::color::Color::from_hex(hex)
                    .ok_or_else(|| ThemeError::Color(name, hex.clone()))?;
                palette.set(token, color);
            }
        }
        Ok(palette)
    }

    /// Resolve into a runnable theme. `complete` marks a bundled file:
    /// it must define every token itself (no base) and carry an
    /// explicit `dark` flag. User files inherit from their base
    /// (default `dark`) and may leave `dark` to inference.
    pub fn resolve(&self, complete: bool) -> Result<Theme, ThemeError> {
        if complete && !self.colors.covers_all() {
            return Err(ThemeError::Incomplete("color tokens"));
        }
        let base_palette = if complete {
            Palette::black()
        } else {
            let base = self.base.as_deref().unwrap_or("dark");
            builtin::palette(base).ok_or_else(|| ThemeError::Base(base.to_string()))?
        };
        let palette = self.palette(base_palette)?;
        let dark = match self.dark {
            Some(dark) => dark,
            None if complete => return Err(ThemeError::Incomplete("dark flag")),
            // An explicit background override is the strongest signal:
            // its luminance decides. Otherwise the base's kind, then dark.
            None if self.colors.background.is_some() => palette.background_luminance() < 0.5,
            None => self.base.as_deref().map(builtin::is_dark).unwrap_or(true),
        };
        let syntax = match &self.syntax_theme {
            Some(alias) => SyntaxTheme::from_alias(alias)
                .ok_or_else(|| ThemeError::SyntaxTheme(alias.clone()))?,
            None => self
                .base
                .as_deref()
                .and_then(builtin::syntax)
                .unwrap_or_else(|| SyntaxTheme::default_for(dark)),
        };
        Ok(Theme::from_parts(dark, palette, syntax))
    }
}

impl ThemeColors {
    /// True when every token carries an explicit override.
    fn covers_all(&self) -> bool {
        ALL_TOKENS
            .iter()
            .all(|token| override_of(self, *token).is_some())
    }
}

/// The override hex for one token, if the file carries one.
fn override_of(colors: &ThemeColors, token: super::tokens::Token) -> Option<&String> {
    use super::tokens::Token;
    match token {
        Token::Primary => colors.primary.as_ref(),
        Token::Accent => colors.accent.as_ref(),
        Token::Text => colors.text.as_ref(),
        Token::TextStrong => colors.text_strong.as_ref(),
        Token::TextDim => colors.text_dim.as_ref(),
        Token::TextMuted => colors.text_muted.as_ref(),
        Token::CodeSpan => colors.code_span.as_ref(),
        Token::Border => colors.border.as_ref(),
        Token::BorderFocus => colors.border_focus.as_ref(),
        Token::Success => colors.success.as_ref(),
        Token::Warning => colors.warning.as_ref(),
        Token::Error => colors.error.as_ref(),
        Token::DiffAdded => colors.diff_added.as_ref(),
        Token::DiffRemoved => colors.diff_removed.as_ref(),
        Token::DiffAddedStrong => colors.diff_added_strong.as_ref(),
        Token::DiffRemovedStrong => colors.diff_removed_strong.as_ref(),
        Token::DiffGutter => colors.diff_gutter.as_ref(),
        Token::DiffMeta => colors.diff_meta.as_ref(),
        Token::RoleUser => colors.role_user.as_ref(),
        Token::ShellMode => colors.shell_mode.as_ref(),
        Token::Neutral => colors.neutral.as_ref(),
        Token::InputBg => colors.input_bg.as_ref(),
        Token::Background => colors.background.as_ref(),
    }
}

/// The JSON key for one token (serde matches this spelling).
pub fn token_name(token: super::tokens::Token) -> &'static str {
    use super::tokens::Token;
    match token {
        Token::Primary => "primary",
        Token::Accent => "accent",
        Token::Text => "text",
        Token::TextStrong => "text_strong",
        Token::TextDim => "text_dim",
        Token::TextMuted => "text_muted",
        Token::CodeSpan => "code_span",
        Token::Border => "border",
        Token::BorderFocus => "border_focus",
        Token::Success => "success",
        Token::Warning => "warning",
        Token::Error => "error",
        Token::DiffAdded => "diff_added",
        Token::DiffRemoved => "diff_removed",
        Token::DiffAddedStrong => "diff_added_strong",
        Token::DiffRemovedStrong => "diff_removed_strong",
        Token::DiffGutter => "diff_gutter",
        Token::DiffMeta => "diff_meta",
        Token::RoleUser => "role_user",
        Token::ShellMode => "shell_mode",
        Token::Neutral => "neutral",
        Token::InputBg => "input_bg",
        Token::Background => "background",
    }
}

/// Load and resolve one user theme by name (the file stem).
pub fn load(home: &Path, name: &str) -> Result<Theme, ThemeError> {
    if !is_valid_theme_name(name) {
        return Err(ThemeError::Name("name", name.to_string()));
    }
    let path = themes_dir(home).join(format!("{name}.json"));
    let text = std::fs::read_to_string(path)?;
    ThemeFile::parse(&text)?.resolve(false)
}

/// Names of the available user themes (file stems, sorted).
pub fn list(home: &Path) -> Vec<String> {
    user_themes(home)
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

/// User theme names paired with their optional picker descriptions.
pub fn describe(home: &Path) -> Vec<(String, Option<String>)> {
    user_themes(home)
}

/// Stems plus descriptions of every parseable `*.json` in the themes
/// directory, sorted. Unparsable files are skipped (the doctor reports
/// them via [`load`]).
fn user_themes(home: &Path) -> Vec<(String, Option<String>)> {
    let mut themes: Vec<(String, Option<String>)> = std::fs::read_dir(themes_dir(home))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let raw = entry.file_name().to_string_lossy().to_string();
            let name = raw.strip_suffix(".json")?.to_string();
            is_valid_theme_name(&name).then_some(name)
        })
        .filter_map(|name| {
            let path = themes_dir(home).join(format!("{name}.json"));
            let text = std::fs::read_to_string(path).ok()?;
            let file = ThemeFile::parse(&text).ok()?;
            Some((name, file.description))
        })
        .collect();
    themes.sort();
    themes
}

/// Theme names share the session-id whitelist: no paths, no escapes.
fn is_valid_theme_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::super::tokens::luminance;
    use super::*;
    use tui_engine::color::Color;

    #[test]
    fn overrides_merge_onto_the_base_palette() {
        let file = ThemeFile::parse(r##"{"colors": {"primary": "#FF0000", "accent": "#00FF00"}}"##)
            .unwrap();
        let resolved = file.resolve(false).unwrap();
        let palette = resolved.palette();
        assert_eq!(palette.primary, Color::rgb(255, 0, 0));
        assert_eq!(palette.accent, Color::rgb(0, 255, 0));
        // Untouched tokens inherit the default base (the dark builtin).
        let base = builtin::palette("dark").unwrap();
        assert_eq!(palette.text, base.text);
        assert_eq!(palette.shell_mode, base.shell_mode);
        assert!(resolved.is_dark(), "base kind inherits");
        assert_eq!(
            resolved.syntax_theme(),
            SyntaxTheme::default_for(true),
            "syntax defaults to the base's"
        );
    }

    #[test]
    fn deepwave_base_pins_the_ocean_identity() {
        let file = ThemeFile::parse(r##"{"base": "deepwave"}"##).unwrap();
        let resolved = file.resolve(false).unwrap();
        assert!(resolved.is_dark());
        assert_eq!(resolved.syntax_theme(), SyntaxTheme::OceanDark);
        let palette = resolved.palette();
        let base = builtin::palette("deepwave").unwrap();
        assert_eq!(palette.primary, base.primary);
    }

    #[test]
    fn dark_flag_defaults_to_the_background_luminance() {
        let file = ThemeFile::parse(
            r##"{"base": "deepwave", "colors": {"background": "#F6F8FA", "text": "#1F2328"}}"##,
        )
        .unwrap();
        assert!(
            !file.resolve(false).unwrap().is_dark(),
            "paper infers light"
        );
    }

    #[test]
    fn syntax_theme_alias_overrides_the_base_derivation() {
        let file = ThemeFile::parse(r##"{"syntax_theme": "ocean-dark"}"##).unwrap();
        let resolved = file.resolve(false).unwrap();
        assert_eq!(resolved.syntax_theme(), SyntaxTheme::OceanDark);
    }

    #[test]
    fn malformed_colors_are_rejected() {
        let file = ThemeFile::parse(r##"{"colors": {"primary": "not-a-color"}}"##).unwrap();
        let error = file.resolve(false).unwrap_err();
        assert!(error.to_string().contains("primary"), "{error}");
    }

    #[test]
    fn unknown_token_names_fail_deserialization() {
        let result = ThemeFile::parse(r##"{"colors": {"primaryy": "#123456"}}"##);
        assert!(result.is_err(), "unknown tokens must not pass silently");
    }

    #[test]
    fn unknown_fields_fail_deserialization() {
        assert!(ThemeFile::parse(r##"{"nope": true}"##).is_err());
    }

    #[test]
    fn unknown_base_names_are_rejected() {
        let file = ThemeFile::parse(r##"{"base": "nope"}"##).unwrap();
        let error = file.resolve(false).unwrap_err();
        assert!(error.to_string().contains("base"), "{error}");
    }

    #[test]
    fn unknown_syntax_theme_alias_is_rejected() {
        let file = ThemeFile::parse(r##"{"syntax_theme": "no-such-theme"}"##).unwrap();
        let error = file.resolve(false).unwrap_err();
        assert!(error.to_string().contains("no-such-theme"), "{error}");
    }

    #[test]
    fn complete_files_reject_missing_tokens_and_kind() {
        // A bundled file must be complete: no base, no silent inherits.
        let file = ThemeFile::parse(r##"{"colors": {"primary": "#123456"}}"##).unwrap();
        assert!(matches!(
            file.resolve(true),
            Err(ThemeError::Incomplete("color tokens"))
        ));
        // Every token spelled out resolves as a complete bundled file.
        let entries: Vec<String> = ALL_TOKENS
            .iter()
            .map(|token| format!("\"{}\": \"#123456\"", token_name(*token)))
            .collect();
        let all_colors = format!("{{\"dark\": true, \"colors\": {{{}}}}}", entries.join(","));
        let file = ThemeFile::parse(&all_colors).unwrap();
        assert!(file.resolve(true).is_ok(), "complete file resolves");
        // The dark flag is mandatory for bundled files.
        let no_kind = all_colors.replacen("\"dark\": true, ", "", 1);
        let file = ThemeFile::parse(&no_kind).unwrap();
        assert!(matches!(
            file.resolve(true),
            Err(ThemeError::Incomplete("dark flag"))
        ));
    }

    #[test]
    fn list_and_load_round_trip_a_theme_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(themes_dir(dir.path())).unwrap();
        std::fs::write(
            themes_dir(dir.path()).join("sunset.json"),
            r##"{"description": "warm dark", "colors": {"role_user": "#FFCB6B"}}"##,
        )
        .unwrap();
        std::fs::write(themes_dir(dir.path()).join("ignored.txt"), "{}").unwrap();
        assert_eq!(list(dir.path()), vec!["sunset".to_string()]);
        assert_eq!(
            describe(dir.path()),
            vec![("sunset".to_string(), Some("warm dark".to_string()))]
        );
        let theme = load(dir.path(), "sunset").unwrap();
        assert!(theme.is_dark());
        assert_eq!(
            theme.color(super::super::tokens::Token::RoleUser),
            Color::rgb(0xFF, 0xCB, 0x6B)
        );
        // Path escapes never reach the filesystem.
        assert!(load(dir.path(), "../escape").is_err());
    }

    #[test]
    fn unparsable_theme_files_are_skipped_by_the_listing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(themes_dir(dir.path())).unwrap();
        std::fs::write(themes_dir(dir.path()).join("good.json"), "{}").unwrap();
        std::fs::write(themes_dir(dir.path()).join("bad.json"), "not json").unwrap();
        assert_eq!(list(dir.path()), vec!["good".to_string()]);
    }

    /// Luminance inference sanity: deep ground vs paper.
    #[test]
    fn luminance_separates_the_kinds() {
        assert!(luminance(Color::rgb(0x1A, 0x1D, 0x23)) < 0.5);
        assert!(luminance(Color::rgb(0xF6, 0xF8, 0xFA)) > 0.5);
    }
}
