//! Static text components.

use crate::color::Style;
use crate::component::Component;

/// One or more pre-styled static lines. Rendering clones the stored
/// lines; styles are painted once at construction, so theme switches
/// must rebuild or invalidate the text via the owning container.
pub struct Text {
    lines: Vec<String>,
}

impl Text {
    /// A single unstyled line.
    pub fn new(line: impl Into<String>) -> Self {
        Self {
            lines: vec![line.into()],
        }
    }

    /// Multiple unstyled lines.
    pub fn multiline(lines: Vec<String>) -> Self {
        Self { lines }
    }

    /// One line painted with `style` (each `\n`-separated row styled
    /// and reset independently).
    pub fn styled(line: impl AsRef<str>, style: Style) -> Self {
        Self {
            lines: line
                .as_ref()
                .split('\n')
                .map(|row| style.paint(row))
                .collect(),
        }
    }

    /// Multiple lines, each painted with `style`.
    pub fn styled_multiline(lines: Vec<String>, style: Style) -> Self {
        Self {
            lines: lines.into_iter().map(|l| style.paint(&l)).collect(),
        }
    }

    /// An empty component rendering zero lines.
    pub fn empty() -> Self {
        Self { lines: Vec::new() }
    }
}

impl Component for Text {
    fn render(&mut self, _width: usize) -> Vec<String> {
        self.lines.clone()
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

/// One blank spacer line.
pub struct Spacer;

impl Component for Spacer {
    fn render(&mut self, _width: usize) -> Vec<String> {
        vec![String::new()]
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

/// Left/right gutter wrapper: indents child lines by `gutter` columns,
/// shrinking the child's render width to keep total width constant.
/// This is the chrome-alignment trick: transcript, panels, and editor
/// all live inside the same one-column gutter so their left edges align.
pub struct Gutter {
    child: Box<dyn Component>,
    gutter: usize,
}

impl Gutter {
    /// Wrap `child`, indenting by `gutter` spaces.
    pub fn new(child: Box<dyn Component>, gutter: usize) -> Self {
        Self { child, gutter }
    }
}

impl Component for Gutter {
    fn render(&mut self, width: usize) -> Vec<String> {
        let inner = width.saturating_sub(self.gutter);
        self.child
            .render(inner)
            .into_iter()
            .map(|line| format!("{}{}", " ".repeat(self.gutter), line))
            .collect()
    }

    fn invalidate(&self) {
        self.child.invalidate();
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn styled_text_paints_each_row() {
        let mut text = Text::styled("hi\nyo", Style::new().bold());
        assert_eq!(
            text.render(80),
            vec![
                "\x1b[1mhi\x1b[0m".to_string(),
                "\x1b[1myo\x1b[0m".to_string()
            ]
        );
    }

    #[test]
    fn gutter_indents_and_shrinks_child_width() {
        struct WidthProbe;
        impl Component for WidthProbe {
            fn render(&mut self, width: usize) -> Vec<String> {
                vec![width.to_string()]
            }

            fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
                self
            }
        }
        let mut gutter = Gutter::new(Box::new(WidthProbe), 2);
        assert_eq!(gutter.render(80), vec!["  78".to_string()]);
    }
}
