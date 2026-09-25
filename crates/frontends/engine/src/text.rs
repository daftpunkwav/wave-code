//! Static text components.

use crate::color::Style;
use crate::component::Component;
use std::sync::Arc;

/// One or more pre-styled static lines. Rendering shares the stored
/// lines by reference (one refcount bump per frame); styles are painted
/// once at construction, so theme switches must rebuild or invalidate
/// the text via the owning container.
pub struct Text {
    lines: Arc<Vec<String>>,
}

impl Text {
    /// A single unstyled line.
    pub fn new(line: impl Into<String>) -> Self {
        Self {
            lines: Arc::new(vec![line.into()]),
        }
    }

    /// Multiple unstyled lines.
    pub fn multiline(lines: Vec<String>) -> Self {
        Self {
            lines: Arc::new(lines),
        }
    }

    /// One line painted with `style` (each `\n`-separated row styled
    /// and reset independently).
    pub fn styled(line: impl AsRef<str>, style: Style) -> Self {
        Self {
            lines: Arc::new(
                line.as_ref()
                    .split('\n')
                    .map(|row| style.paint(row))
                    .collect(),
            ),
        }
    }

    /// Multiple lines, each painted with `style`.
    pub fn styled_multiline(lines: Vec<String>, style: Style) -> Self {
        Self {
            lines: Arc::new(lines.into_iter().map(|l| style.paint(&l)).collect()),
        }
    }

    /// An empty component rendering zero lines.
    pub fn empty() -> Self {
        Self {
            lines: Arc::new(Vec::new()),
        }
    }
}

impl Component for Text {
    fn render(&mut self, _width: usize) -> Arc<Vec<String>> {
        Arc::clone(&self.lines)
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

/// One blank spacer line.
pub struct Spacer;

impl Component for Spacer {
    fn render(&mut self, _width: usize) -> Arc<Vec<String>> {
        Arc::new(vec![String::new()])
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
    fn render(&mut self, width: usize) -> Arc<Vec<String>> {
        // Indentation rewrites every line: a transforming wrapper owns
        // its output (nothing upstream shares it).
        let inner = width.saturating_sub(self.gutter);
        let lines: Vec<String> = self
            .child
            .render(inner)
            .iter()
            .map(|line| format!("{}{}", " ".repeat(self.gutter), line))
            .collect();
        Arc::new(lines)
    }

    fn invalidate(&mut self) {
        self.child.invalidate();
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::component::Segment;

    #[test]
    fn styled_text_paints_each_row() {
        let mut text = Text::styled("hi\nyo", Style::new().bold());
        assert_eq!(
            *text.render(80),
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
            fn render(&mut self, width: usize) -> Segment {
                Segment::new(vec![width.to_string()])
            }

            fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
                self
            }
        }
        let mut gutter = Gutter::new(Box::new(WidthProbe), 2);
        assert_eq!(*gutter.render(80), vec!["  78".to_string()]);
    }
}
