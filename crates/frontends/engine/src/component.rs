//! The component model: retained objects that render to ANSI lines.
//!
//! A component owns its state and produces the ANSI-styled strings for
//! its terminal rows at a given width, mirroring the immediate-retained
//! hybrid of the reference design: the screen layer diffs those line
//! arrays; components may cache their output behind
//! [`Component::invalidate`].
//!
//! Render output is a [`Segment`] — one reference-counted line array.
//! Cache hits hand back the same allocation (a refcount bump) instead
//! of deep-copying every line per frame; the screen layer keeps the
//! previous frame as segments too, so unchanged segments diff by
//! pointer equality.

use crate::keys::KeyEvent;
use std::sync::Arc;

/// One component's rendered lines, shared by reference across frames.
pub type Segment = Arc<Vec<String>>;

/// A renderable piece of the interface.
pub trait Component {
    /// Produce the lines of this component at `width`. One string per
    /// terminal row; callers diff these arrays between frames.
    fn render(&mut self, width: usize) -> Segment;

    /// Drop any cached render state; the next render recomputes. Theme
    /// and render-mode switches call this on every live component so
    /// cached lines never carry a stale palette or mode.
    fn invalidate(&mut self) {}

    /// Typed access for callers that store mixed children and must
    /// update specific component types in place (e.g. tool cards).
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;
}

/// A component that accepts keyboard input while focused.
pub trait Focusable: Component {
    /// Handle one key event. Returns true when the event was consumed.
    fn handle_key(&mut self, event: KeyEvent) -> bool;

    /// Notification of focus changes (border highlights, cursor shape).
    fn set_focused(&mut self, focused: bool);
}

/// A vertical composition of child components.
#[derive(Default)]
pub struct Container {
    children: Vec<Box<dyn Component>>,
}

impl Container {
    /// An empty container.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a child; renders concatenate in insertion order.
    pub fn push(&mut self, child: Box<dyn Component>) {
        self.children.push(child);
    }

    /// Number of children.
    pub fn len(&self) -> usize {
        self.children.len()
    }

    /// True when there are no children.
    pub fn is_empty(&self) -> bool {
        self.children.is_empty()
    }

    /// Borrow the child at `index`.
    pub fn get(&self, index: usize) -> Option<&dyn Component> {
        self.children.get(index).map(|c| c.as_ref())
    }

    /// Mutably borrow the child at `index` (focus routing in the app
    /// layer downcasts to concrete input components).
    pub fn get_mut(&mut self, index: usize) -> Option<&mut dyn Component> {
        match self.children.get_mut(index) {
            Some(child) => Some(child.as_mut()),
            None => None,
        }
    }

    /// Remove and return the last child.
    pub fn pop(&mut self) -> Option<Box<dyn Component>> {
        self.children.pop()
    }

    /// Remove every child.
    pub fn clear(&mut self) {
        self.children.clear();
    }
}

impl Component for Container {
    fn render(&mut self, width: usize) -> Segment {
        // Concatenation materializes one array per frame; containers
        // with cacheable children should sit behind a caching parent
        // instead (the transcript does).
        let mut lines = Vec::new();
        for child in &mut self.children {
            lines.extend(child.render(width).iter().cloned());
        }
        Arc::new(lines)
    }

    fn invalidate(&mut self) {
        for child in &mut self.children {
            child.invalidate();
        }
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::Text;

    #[test]
    fn container_concatenates_children() {
        let mut container = Container::new();
        container.push(Box::new(Text::new("a")));
        container.push(Box::new(Text::new("b")));
        assert_eq!(
            *container.render(80),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn empty_container_renders_nothing() {
        let mut container = Container::new();
        assert!(container.render(80).is_empty());
    }
}
