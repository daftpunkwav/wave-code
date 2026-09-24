//! Custom themes from `~/.wavecode/themes/<name>.json`.
//!
//! A theme file names a base (`dark` or `light`) and optionally
//! overrides any of the semantic tokens with `#rrggbb` hex values;
//! unknown token names and malformed colors are rejected so a typo
//! never silently renders as the base palette.

use std::path::{Path, PathBuf};

use super::colors::{Palette, dark_palette, light_palette};

/// Themes root directory under the home directory.
pub fn themes_dir(home: &Path) -> PathBuf {
    home.join(".wavecode").join("themes")
}

/// The base palette a custom theme builds on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeBase {
    Dark,
    Light,
}

/// One custom theme file: base palette plus per-token hex overrides.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct CustomTheme {
    /// The palette the overrides apply to.
    pub base: ThemeBase,
    /// Per-token hex overrides (`#rrggbb`); unknown names are rejected
    /// by [`CustomTheme::palette`].
    #[serde(default)]
    pub colors: ThemeColors,
}

/// Every semantic token, each overridable independently. Unknown names
/// are rejected: a typoed token must not silently render as the base.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
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
}

impl std::fmt::Display for ThemeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ThemeError::Io(error) => write!(f, "theme IO failed: {error}"),
            ThemeError::Json(error) => write!(f, "theme JSON failed: {error}"),
            ThemeError::Color(name, value) => write!(f, "invalid color for {name}: {value}"),
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

impl CustomTheme {
    /// Merge the overrides onto the base palette.
    pub fn palette(&self) -> Result<Palette, ThemeError> {
        let mut palette = match self.base {
            ThemeBase::Dark => dark_palette(),
            ThemeBase::Light => light_palette(),
        };
        let apply = |slot: &mut tui_engine::color::Color,
                     name: &'static str,
                     value: &Option<String>|
         -> Result<(), ThemeError> {
            if let Some(hex) = value {
                *slot = tui_engine::color::Color::from_hex(hex)
                    .ok_or_else(|| ThemeError::Color(name, hex.clone()))?;
            }
            Ok(())
        };
        apply(&mut palette.primary, "primary", &self.colors.primary)?;
        apply(&mut palette.accent, "accent", &self.colors.accent)?;
        apply(&mut palette.text, "text", &self.colors.text)?;
        apply(
            &mut palette.text_strong,
            "text_strong",
            &self.colors.text_strong,
        )?;
        apply(&mut palette.text_dim, "text_dim", &self.colors.text_dim)?;
        apply(
            &mut palette.text_muted,
            "text_muted",
            &self.colors.text_muted,
        )?;
        apply(&mut palette.code_span, "code_span", &self.colors.code_span)?;
        apply(&mut palette.border, "border", &self.colors.border)?;
        apply(
            &mut palette.border_focus,
            "border_focus",
            &self.colors.border_focus,
        )?;
        apply(&mut palette.success, "success", &self.colors.success)?;
        apply(&mut palette.warning, "warning", &self.colors.warning)?;
        apply(&mut palette.error, "error", &self.colors.error)?;
        apply(
            &mut palette.diff_added,
            "diff_added",
            &self.colors.diff_added,
        )?;
        apply(
            &mut palette.diff_removed,
            "diff_removed",
            &self.colors.diff_removed,
        )?;
        apply(
            &mut palette.diff_added_strong,
            "diff_added_strong",
            &self.colors.diff_added_strong,
        )?;
        apply(
            &mut palette.diff_removed_strong,
            "diff_removed_strong",
            &self.colors.diff_removed_strong,
        )?;
        apply(
            &mut palette.diff_gutter,
            "diff_gutter",
            &self.colors.diff_gutter,
        )?;
        apply(&mut palette.diff_meta, "diff_meta", &self.colors.diff_meta)?;
        apply(&mut palette.role_user, "role_user", &self.colors.role_user)?;
        apply(
            &mut palette.shell_mode,
            "shell_mode",
            &self.colors.shell_mode,
        )?;
        Ok(palette)
    }
}

/// Load and resolve one custom theme by name (the file stem).
pub fn load(home: &Path, name: &str) -> Result<super::active::Theme, ThemeError> {
    if !is_valid_theme_name(name) {
        return Err(ThemeError::Color("name", name.to_string()));
    }
    let path = themes_dir(home).join(format!("{name}.json"));
    let text = std::fs::read_to_string(path)?;
    let custom: CustomTheme = serde_json::from_str(&text)?;
    let palette = custom.palette()?;
    Ok(super::active::Theme::from_palette(
        custom.base == ThemeBase::Dark,
        palette,
    ))
}

/// Names of the available custom themes (file stems, sorted).
pub fn list(home: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(themes_dir(home))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            name.strip_suffix(".json").map(str::to_string)
        })
        .filter(|name| is_valid_theme_name(name))
        .collect();
    names.sort();
    names
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
    use super::*;

    #[test]
    fn overrides_merge_onto_the_base_palette() {
        let theme: CustomTheme = serde_json::from_str(
            r##"{ "base": "dark", "colors": { "primary": "#FF0000", "accent": "#00FF00" } }"##,
        )
        .unwrap();
        let palette = theme.palette().unwrap();
        let base = dark_palette();
        assert_eq!(palette.primary, tui_engine::color::Color::rgb(255, 0, 0));
        assert_eq!(palette.accent, tui_engine::color::Color::rgb(0, 255, 0));
        // Untouched tokens keep the base values.
        assert_eq!(palette.text, base.text);
        assert_eq!(palette.shell_mode, base.shell_mode);
    }

    #[test]
    fn malformed_colors_are_rejected() {
        let theme: CustomTheme = serde_json::from_str(
            r##"{ "base": "light", "colors": { "primary": "not-a-color" } }"##,
        )
        .unwrap();
        let error = theme.palette().unwrap_err();
        assert!(error.to_string().contains("primary"), "{error}");
    }

    #[test]
    fn unknown_token_names_fail_deserialization() {
        let result: Result<CustomTheme, _> =
            serde_json::from_str(r##"{ "base": "dark", "colors": { "primaryy": "#123456" } }"##);
        assert!(result.is_err(), "unknown tokens must not pass silently");
    }

    #[test]
    fn list_and_load_round_trip_a_theme_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(themes_dir(dir.path())).unwrap();
        std::fs::write(
            themes_dir(dir.path()).join("sunset.json"),
            r##"{ "base": "dark", "colors": { "role_user": "#FFCB6B" } }"##,
        )
        .unwrap();
        std::fs::write(themes_dir(dir.path()).join("ignored.txt"), "{}").unwrap();
        assert_eq!(list(dir.path()), vec!["sunset".to_string()]);
        let theme = load(dir.path(), "sunset").unwrap();
        assert!(theme.is_dark());
        assert!(
            theme.color(super::super::colors::Token::RoleUser)
                == tui_engine::color::Color::rgb(0xFF, 0xCB, 0x6B)
        );
        // Path escapes never reach the filesystem.
        assert!(load(dir.path(), "../escape").is_err());
    }
}
