//! Color and SGR (Select Graphic Rendition) escape helpers.
//!
//! The engine paints with SGR sequences at the installed
//! [`ColorDepth`] (truecolor by default); the application layer maps
//! its semantic tokens onto [`Color`] values and degrades the depth
//! once at startup for terminals that cannot pass 24-bit color
//! through. Every styled span must end with [`RESET`] so styles never
//! leak across lines.

use std::sync::atomic::{AtomicU8, Ordering};

/// Terminal color capability, resolved once at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorDepth {
    /// 24-bit foreground: `38;2;r;g;b`.
    #[default]
    TrueColor,
    /// Nearest xterm-256 palette index: `38;5;idx`.
    Color256,
    /// Nearest of the 16 classic ANSI colors: `3n` / `9n`.
    Ansi16,
}

static DEPTH: AtomicU8 = AtomicU8::new(0);

/// Install the startup color depth (call once, before rendering).
pub fn set_color_depth(depth: ColorDepth) {
    let code = match depth {
        ColorDepth::TrueColor => 0,
        ColorDepth::Color256 => 1,
        ColorDepth::Ansi16 => 2,
    };
    DEPTH.store(code, Ordering::Relaxed);
}

/// The installed color depth.
pub fn color_depth() -> ColorDepth {
    match DEPTH.load(Ordering::Relaxed) {
        1 => ColorDepth::Color256,
        2 => ColorDepth::Ansi16,
        _ => ColorDepth::TrueColor,
    }
}

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

    /// Foreground SGR parameters at the installed [`ColorDepth`]:
    /// `38;2;r;g;b` (truecolor), `38;5;idx` (256), or `3n`/`9n` (16).
    pub fn fg_params(&self) -> String {
        self.fg_params_for_depth(color_depth())
    }

    /// Foreground SGR parameters under an explicit depth.
    pub fn fg_params_for_depth(&self, depth: ColorDepth) -> String {
        match depth {
            ColorDepth::TrueColor => format!("38;2;{};{};{}", self.r, self.g, self.b),
            ColorDepth::Color256 => format!("38;5;{}", self.nearest_xterm256()),
            ColorDepth::Ansi16 => ansi16_fg_params(self.nearest_ansi16()),
        }
    }

    /// Nearest xterm-256 palette index: the 24-step grayscale ramp for
    /// neutral colors, the 6x6x6 cube otherwise.
    fn nearest_xterm256(self) -> u8 {
        if self.r == self.g && self.g == self.b {
            if self.r < 8 {
                return 16; // cube black
            }
            if self.r > 248 {
                return 231; // cube white
            }
            return 232 + (self.r - 8) / 10;
        }
        let quantize = |v: u8| ((v as f32 / 255.0 * 5.0).round() as u16).min(5) as u8;
        16 + 36 * quantize(self.r) + 6 * quantize(self.g) + quantize(self.b)
    }

    /// Nearest of the 16 classic ANSI colors (0-15), decided by
    /// saturation/value band and then hue. Plain gamma-space RGB
    /// distance collapses mid-saturation hues onto gray, so the bands
    /// are deliberate: neutrals stay gray, hues keep their family, and
    /// brighter colors take the bright variant.
    fn nearest_ansi16(self) -> u8 {
        let (r, g, b) = (self.r as f32, self.g as f32, self.b as f32);
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let value = max / 255.0;
        let saturation = if max == 0.0 { 0.0 } else { (max - min) / max };
        if value < 0.12 {
            return 0; // black
        }
        if saturation < 0.28 {
            if value < 0.45 {
                return 8; // bright black
            }
            if value <= 0.82 {
                return 7; // silver
            }
            return 15; // white
        }
        let span = max - min;
        let mut hue = if max == r {
            (g - b) / span
        } else if max == g {
            2.0 + (b - r) / span
        } else {
            4.0 + (r - g) / span
        } * 60.0;
        if hue < 0.0 {
            hue += 360.0;
        }
        let bright = value >= 0.55;
        let (bright_index, dim_index) = match hue {
            h if !(20.0..330.0).contains(&h) => (9, 1), // red
            h if h < 70.0 => (11, 3),                   // yellow
            h if h < 160.0 => (10, 2),                  // green
            h if h < 200.0 => (14, 6),                  // cyan
            // The blue band ends at 260 so blue-violets degrade to
            // magenta, keeping them distinct from blues.
            h if h < 260.0 => (12, 4), // blue
            _ => (13, 5),              // magenta
        };
        if bright { bright_index } else { dim_index }
    }
}

/// Foreground SGR parameters for an ANSI-16 index (0-15).
fn ansi16_fg_params(index: u8) -> String {
    if index < 8 {
        format!("3{index}")
    } else {
        format!("9{}", index - 8)
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

    /// Serializes tests that flip the global color depth against tests
    /// that assert truecolor output.
    static DEPTH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
        let _guard = DEPTH_LOCK.lock().unwrap();
        let previous = color_depth();
        set_color_depth(ColorDepth::TrueColor);
        let style = Style::new().fg(Color::rgb(0x10, 0x20, 0x30)).bold();
        assert_eq!(
            style.paint("hi"),
            "\x1b[38;2;16;32;48;1mhi\x1b[0m".to_string()
        );
        set_color_depth(previous);
    }

    #[test]
    fn plain_style_adds_no_sequences() {
        assert!(Style::new().is_plain());
        assert_eq!(Style::new().paint("hi"), "hi");
    }

    /// The deepwave dark roles degrade to the expected ANSI-16 family.
    #[test]
    fn ansi16_mapping_keeps_hue_families() {
        let cases = [
            ("#2DD4BF", 14), // primary teal -> bright cyan
            ("#22D3EE", 14), // role_user cyan -> bright cyan
            ("#60A5FA", 12), // accent azure -> bright blue
            ("#5FB878", 10), // success sea-green -> bright green
            ("#E5C07B", 11), // warning amber -> bright yellow
            ("#D8B871", 11), // code sand -> bright yellow
            ("#E06C75", 9),  // error coral -> bright red
            ("#BD93F9", 13), // shell blue-violet -> bright magenta
            ("#7C3AED", 13), // light-shell violet -> bright magenta
            ("#D8E1E8", 15), // text -> white
            ("#8B9BB4", 7),  // dim slate -> silver
            ("#4A5866", 8),  // gutter -> bright black
            ("#0F1A1E", 0),  // near black -> black
        ];
        for (hex, expected) in cases {
            let color = Color::from_hex(hex).unwrap();
            assert_eq!(color.nearest_ansi16(), expected, "{hex}");
        }
    }

    #[test]
    fn ansi16_foreground_params_use_3n_and_9n() {
        assert_eq!(ansi16_fg_params(0), "30");
        assert_eq!(ansi16_fg_params(7), "37");
        assert_eq!(ansi16_fg_params(14), "96");
    }

    #[test]
    fn xterm256_maps_gray_ramp_and_cube() {
        assert_eq!(Color::rgb(0, 0, 0).nearest_xterm256(), 16);
        assert_eq!(Color::rgb(255, 255, 255).nearest_xterm256(), 231);
        assert_eq!(Color::rgb(128, 128, 128).nearest_xterm256(), 244);
        // Pure red lands on the classic cube corner 5-0-0.
        assert_eq!(Color::rgb(255, 0, 0).nearest_xterm256(), 196);
    }

    #[test]
    fn depth_gate_switches_fg_params() {
        let _guard = DEPTH_LOCK.lock().unwrap();
        let previous = color_depth();
        let color = Color::rgb(0x2D, 0xD4, 0xBF);
        set_color_depth(ColorDepth::TrueColor);
        assert_eq!(color.fg_params(), "38;2;45;212;191");
        set_color_depth(ColorDepth::Color256);
        assert_eq!(color.fg_params(), "38;5;80");
        set_color_depth(ColorDepth::Ansi16);
        assert_eq!(color.fg_params(), "96");
        set_color_depth(previous);
    }
}
