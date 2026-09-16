//! Spinner (loader) component: animated frame + label.
//!
//! Two frame sets, matching the reference timing: braille dots at 80 ms
//! and moon phases at 120 ms. The frame is derived from elapsed time, so
//! no background timer is needed — the render loop drives animation.

use std::time::Instant;

use crate::color::Style;
use crate::component::Component;

/// Braille dot spinner frames (80 ms interval).
pub const BRAILLE_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Braille spinner frame interval in milliseconds.
pub const BRAILLE_INTERVAL_MS: u64 = 80;

/// Moon phase spinner frames (120 ms interval).
pub const MOON_FRAMES: [&str; 8] = ["🌑", "🌒", "🌓", "🌔", "🌕", "🌖", "🌗", "🌘"];
/// Moon spinner frame interval in milliseconds.
pub const MOON_INTERVAL_MS: u64 = 120;

/// The spinner glyph set to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpinnerStyle {
    /// Braille dots (working/composing indicator).
    Braille,
    /// Moon phases (waiting/tool indicator).
    Moon,
}

impl SpinnerStyle {
    fn frames(self) -> &'static [&'static str] {
        match self {
            Self::Braille => &BRAILLE_FRAMES,
            Self::Moon => &MOON_FRAMES,
        }
    }

    fn interval(self) -> std::time::Duration {
        match self {
            Self::Braille => std::time::Duration::from_millis(BRAILLE_INTERVAL_MS),
            Self::Moon => std::time::Duration::from_millis(MOON_INTERVAL_MS),
        }
    }
}

/// An animated spinner line: `<frame> <label>`.
pub struct Loader {
    style: SpinnerStyle,
    frame_style: Style,
    label_style: Style,
    label: String,
    started: Instant,
}

impl Loader {
    /// A spinner animating from now.
    pub fn new(
        style: SpinnerStyle,
        label: impl Into<String>,
        frame_style: Style,
        label_style: Style,
    ) -> Self {
        Self {
            style,
            frame_style,
            label_style,
            label: label.into(),
            started: Instant::now(),
        }
    }

    /// Replace the label text (e.g. "working…" → "Retrying (attempt 2)…").
    pub fn set_label(&mut self, label: impl Into<String>) {
        self.label = label.into();
    }

    /// The current frame glyph (exposed for inline embedding in other
    /// components, e.g. the activity pane).
    pub fn current_frame(&self) -> &'static str {
        let frames = self.style.frames();
        let elapsed = self.started.elapsed().as_millis() as u64;
        let index = (elapsed / self.style.interval().as_millis() as u64) as usize;
        frames[index % frames.len()]
    }
}

impl Component for Loader {
    fn render(&mut self, _width: usize) -> Vec<String> {
        let frame = self.current_frame();
        vec![format!(
            "{} {}",
            self.frame_style.paint(frame),
            self.label_style.paint(&self.label)
        )]
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::width::strip_ansi;

    #[test]
    fn frame_advances_with_elapsed_time() {
        let mut loader = Loader::new(SpinnerStyle::Braille, "working", Style::new(), Style::new());
        let first = loader.current_frame();
        assert_eq!(strip_ansi(&loader.render(80)[0]), "⠋ working");
        std::thread::sleep(std::time::Duration::from_millis(90));
        assert_ne!(loader.current_frame(), first);
    }

    #[test]
    fn moon_style_starts_at_new_moon() {
        let loader = Loader::new(SpinnerStyle::Moon, "waiting", Style::new(), Style::new());
        assert_eq!(loader.current_frame(), "🌑");
    }
}
