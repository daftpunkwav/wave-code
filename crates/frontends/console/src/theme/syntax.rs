//! Syntax theme selection: the palette's code-coloring counterpart.
//!
//! Selecting a chrome theme selects its syntect theme too; custom
//! themes may override the choice with a `syntax_theme` alias. The
//! aliases map onto the highlighter's theme set (see
//! [`crate::highlight`]).

/// A registered syntax highlighting theme, addressed by alias.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyntaxTheme {
    /// The bundled SynthWave '84 tmTheme: the synthwave identity.
    Synthwave84,
    /// `base16-ocean.dark`: the deepwave identity's ocean syntax.
    OceanDark,
    /// `base16-ocean.light`: the light identity's ocean syntax.
    OceanLight,
}

impl SyntaxTheme {
    /// The name this theme carries in the highlighter's theme set.
    pub fn name(self) -> &'static str {
        match self {
            SyntaxTheme::Synthwave84 => "synthwave-84",
            SyntaxTheme::OceanDark => "base16-ocean.dark",
            SyntaxTheme::OceanLight => "base16-ocean.light",
        }
    }

    /// Resolve a custom-theme `syntax_theme` alias. Unknown aliases are
    /// rejected by the loader, never silently ignored.
    pub fn from_alias(alias: &str) -> Option<Self> {
        match alias {
            "synthwave-84" => Some(SyntaxTheme::Synthwave84),
            "ocean-dark" => Some(SyntaxTheme::OceanDark),
            "ocean-light" => Some(SyntaxTheme::OceanLight),
            _ => None,
        }
    }

    /// The `syntax_theme` alias (the inverse of [`Self::from_alias`]).
    pub fn alias(self) -> &'static str {
        match self {
            SyntaxTheme::Synthwave84 => "synthwave-84",
            SyntaxTheme::OceanDark => "ocean-dark",
            SyntaxTheme::OceanLight => "ocean-light",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_round_trip() {
        for theme in [
            SyntaxTheme::Synthwave84,
            SyntaxTheme::OceanDark,
            SyntaxTheme::OceanLight,
        ] {
            assert_eq!(SyntaxTheme::from_alias(theme.alias()), Some(theme));
        }
        assert_eq!(SyntaxTheme::from_alias("nope"), None);
    }

    #[test]
    fn names_match_the_highlighter_set() {
        // base16 names are syntect's own; the bundled theme uses its
        // registered name. The highlighter test locks the actual set.
        assert_eq!(SyntaxTheme::OceanDark.name(), "base16-ocean.dark");
        assert_eq!(SyntaxTheme::OceanLight.name(), "base16-ocean.light");
        assert_eq!(SyntaxTheme::Synthwave84.name(), "synthwave-84");
    }
}
