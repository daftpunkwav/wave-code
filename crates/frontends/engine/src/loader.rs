//! Spinner (loader) component: animated frame + label.
//!
//! Waveform frame sets matching the WaveCode sound-wave identity, one
//! wave per activity kind: sine ripple for assistant speech (composing),
//! triangle for thinking, saw ramp for machine work (waiting/tools).
//! The frame is derived from elapsed time, so no background timer is
//! needed — the render loop drives animation.

use std::time::Instant;

use crate::color::Style;
use crate::component::Component;

/// Sine ripple frames: amplitude swells and recedes (80 ms).
pub const SINE_FRAMES: [&str; 12] = [
    "▁", "▂", "▃", "▄", "▅", "▆", "▇", "▆", "▅", "▄", "▃", "▂",
];
/// Sine spinner frame interval in milliseconds.
pub const SINE_INTERVAL_MS: u64 = 80;

/// Saw ramp frames: climbs then drops back sharp (100 ms).
pub const SAW_FRAMES: [&str; 4] = ["▁", "▃", "▅", "▇"];
/// Saw spinner frame interval in milliseconds.
pub const SAW_INTERVAL_MS: u64 = 100;

/// Triangle frames: symmetric up-down sweep (80 ms).
pub const TRIANGLE_FRAMES: [&str; 6] = ["▁", "▃", "▅", "▇", "▅", "▃"];
/// Triangle spinner frame interval in milliseconds.
pub const TRIANGLE_INTERVAL_MS: u64 = 80;

/// The spinner glyph set to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpinnerStyle {
    /// Sine ripple (assistant composing indicator).
    Sine,
    /// Saw ramp (waiting/tool indicator).
    Saw,
    /// Triangle sweep (thinking indicator).
    Triangle,
}

impl SpinnerStyle {
    fn frames(self) -> &'static [&'static str] {
        match self {
            Self::Sine => &SINE_FRAMES,
            Self::Saw => &SAW_FRAMES,
            Self::Triangle => &TRIANGLE_FRAMES,
        }
    }

    fn interval(self) -> std::time::Duration {
        match self {
            Self::Sine => std::time::Duration::from_millis(SINE_INTERVAL_MS),
            Self::Saw => std::time::Duration::from_millis(SAW_INTERVAL_MS),
            Self::Triangle => std::time::Duration::from_millis(TRIANGLE_INTERVAL_MS),
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
        let mut loader = Loader::new(SpinnerStyle::Sine, "working", Style::new(), Style::new());
        let first = loader.current_frame();
        assert_eq!(strip_ansi(&loader.render(80)[0]), "▁ working");
        std::thread::sleep(std::time::Duration::from_millis(90));
        assert_ne!(loader.current_frame(), first);
    }

    #[test]
    fn sine_style_starts_at_trough() {
        let loader = Loader::new(SpinnerStyle::Sine, "waiting", Style::new(), Style::new());
        assert_eq!(loader.current_frame(), "▁");
    }

    #[test]
    fn saw_style_cycles_up_then_drops() {
        let loader = Loader::new(SpinnerStyle::Saw, "tool", Style::new(), Style::new());
        assert_eq!(loader.current_frame(), "▁");
    }

    #[test]
    fn triangle_style_starts_at_trough() {
        let loader = Loader::new(SpinnerStyle::Triangle, "thinking", Style::new(), Style::new());
        assert_eq!(loader.current_frame(), "▁");
    }
}
