//! 24-bit color and SGR (Select Graphic Rendition) escape helpers.
//!
//! The engine paints with truecolor SGR sequences; the application layer
//! maps its semantic tokens onto [`Color`] values. Every styled span must
//! end with [`RESET`] so styles never leak across lines.

/// A 24-bit RGB color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    /// Red channel, 0-255.
    pub r: u8,
    /// Green channel, 0-255.
    pub g: u8,
    /// Blue channel, 0-255.
    pub b: u8,
}

impl Color {
    /// Build a color from channel values.
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Parse a `#rrggbb` or `rrggbb` hex string (case-insensitive).
    /// Returns `None` on any malformed input; callers treat that as
    /// "keep the default" per the custom-theme contract.
    pub fn from_hex(hex: &str) -> Option<Self> {
        let hex = hex.strip_prefix('#').unwrap_or(hex);
        if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let byte = |range: std::ops::Range<usize>| u8::from_str_radix(&hex[range], 16).ok();
        Some(Self {
            r: byte(0..2)?,
            g: byte(2..4)?,
            b: byte(4..6)?,
        })
    }

    /// Foreground SGR parameters: `38;2;r;g;b`.
    pub fn fg_params(&self) -> String {
        format!("38;2;{};{};{}", self.r, self.g, self.b)
    }
}

/// SGR reset ending every styled span.
pub const RESET: &str = "\x1b[0m";

/// A foreground style: optional color plus emphasis flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    /// Foreground color; `None` keeps the terminal default.
    pub fg: Option<Color>,
    /// Bold or increased-intensity text.
    pub bold: bool,
    /// Dim or decreased-intensity text.
    pub dim: bool,
    /// Italic text.
    pub italic: bool,
    /// Underlined text.
    pub underline: bool,
    /// Struck-through text (completed todo rows).
    pub strikethrough: bool,
    /// Reversed video (used for the in-box cursor).
    pub reverse: bool,
}

impl Style {
    /// Default style (terminal foreground, no emphasis).
    pub const fn new() -> Self {
        Self {
            fg: None,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            strikethrough: false,
            reverse: false,
        }
    }

    /// Set the foreground color.
    pub const fn fg(mut self, color: Color) -> Self {
        self.fg = Some(color);
        self
    }

    /// Enable bold.
    pub const fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    /// Enable dim.
    pub const fn dim(mut self) -> Self {
        self.dim = true;
        self
    }

    /// Enable italic.
    pub const fn italic(mut self) -> Self {
        self.italic = true;
        self
    }

    /// Enable underline.
    pub const fn underline(mut self) -> Self {
        self.underline = true;
        self
    }

    /// Enable strikethrough.
    pub const fn strikethrough(mut self) -> Self {
        self.strikethrough = true;
        self
    }

    /// SGR parameter list for this style, or the empty string when the
    /// style is the default (no sequence emitted at all).
    pub fn sgr(&self) -> String {
        let mut params: Vec<String> = Vec::new();
        if let Some(fg) = self.fg {
            params.push(fg.fg_params());
        }
        if self.bold {
            params.push("1".to_string());
        }
        if self.dim {
            params.push("2".to_string());
        }
        if self.italic {
            params.push("3".to_string());
        }
        if self.underline {
            params.push("4".to_string());
        }
        if self.strikethrough {
            params.push("9".to_string());
        }
        if self.reverse {
            params.push("7".to_string());
        }
        params.join(";")
    }

    /// True when this style emits no SGR sequence.
    pub fn is_plain(&self) -> bool {
        self.sgr().is_empty()
    }

    /// Wrap `text` in this style followed by a reset. Plain styles pass
    /// the text through untouched so zero-width sequences are not added.
    pub fn paint(&self, text: &str) -> String {
        if text.is_empty() {
            return String::new();
        }
        let sgr = self.sgr();
        if sgr.is_empty() {
            return text.to_string();
        }
        format!("\x1b[{sgr}m{text}{RESET}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_parsing_accepts_valid_forms() {
        assert_eq!(
            Color::from_hex("#4FA8FF"),
            Some(Color::rgb(0x4F, 0xA8, 0xFF))
        );
        assert_eq!(
            Color::from_hex("4fa8ff"),
            Some(Color::rgb(0x4F, 0xA8, 0xFF))
        );
        assert_eq!(Color::from_hex("#4FA8F"), None);
        assert_eq!(Color::from_hex("4fa8ffz"), None);
        assert_eq!(Color::from_hex(""), None);
    }

    #[test]
    fn paint_wraps_with_sgr_and_reset() {
        let style = Style::new().fg(Color::rgb(0x10, 0x20, 0x30)).bold();
        assert_eq!(
            style.paint("hi"),
            "\x1b[38;2;16;32;48;1mhi\x1b[0m".to_string()
        );
    }

    #[test]
    fn plain_style_adds_no_sequences() {
        assert!(Style::new().is_plain());
        assert_eq!(Style::new().paint("hi"), "hi");
    }
}
