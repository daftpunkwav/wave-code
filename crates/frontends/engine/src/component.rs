//! The component model: retained objects that render to ANSI lines.
//!
//! A component owns its state and produces one ANSI-styled string per
//! terminal line for a given width (`render(width) -> Vec<String>`),
//! mirroring the immediate-retained hybrid of the reference design: the
//! screen layer diffs those line arrays; components may cache their
//! output behind [`Component::invalidate`].

use crate::keys::KeyEvent;

/// A renderable piece of the interface.
pub trait Component {
    /// Produce the lines of this component at `width`. One string per
    /// terminal row; callers diff these arrays between frames.
    fn render(&mut self, width: usize) -> Vec<String>;

    /// Drop any cached render state; the next render recomputes.
    fn invalidate(&self) {}

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
    fn render(&mut self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        for child in &mut self.children {
            lines.extend(child.render(width));
        }
        lines
    }

    fn invalidate(&self) {
        for child in &self.children {
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
        assert_eq!(container.render(80), vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn empty_container_renders_nothing() {
        let mut container = Container::new();
        assert!(container.render(80).is_empty());
    }
}
