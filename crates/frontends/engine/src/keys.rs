//! Normalized key model shared by the editor, dialogs, and the app loop.
//!
//! Input arrives as crossterm events (optionally enhanced by the Kitty
//! keyboard protocol); this module folds them into [`KeyEvent`] values
//! with a stable [`Key`] vocabulary so component code never parses
//! terminal sequences.

/// A logical key, independent of the terminal's encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// A printable character (after ctrl/alt normalization of letters).
    Char(char),
    /// Enter / Return.
    Enter,
    /// Tab (shift state is carried on the event, not the key).
    Tab,
    /// Backspace.
    Backspace,
    /// Delete.
    Delete,
    /// Escape.
    Esc,
    /// Arrow left.
    Left,
    /// Arrow right.
    Right,
    /// Arrow up.
    Up,
    /// Arrow down.
    Down,
    /// Home.
    Home,
    /// End.
    End,
    /// Page up.
    PageUp,
    /// Page down.
    PageDown,
    /// Insert.
    Insert,
    /// A function key F1-F12.
    F(u8),
    /// Any key this model does not represent.
    Other,
}

/// Modifier flags carried with a key press.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mods {
    /// Ctrl held.
    pub ctrl: bool,
    /// Alt (or Option) held.
    pub alt: bool,
    /// Shift held.
    pub shift: bool,
}

impl Mods {
    /// No modifiers held.
    pub const NONE: Mods = Mods {
        ctrl: false,
        alt: false,
        shift: false,
    };

    /// Ctrl held.
    pub const CTRL: Mods = Mods {
        ctrl: true,
        alt: false,
        shift: false,
    };
}

/// One key event: a key plus its modifier state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyEvent {
    /// The logical key.
    pub key: Key,
    /// Held modifiers.
    pub mods: Mods,
}

impl KeyEvent {
    /// Build an event with explicit modifiers.
    pub const fn new(key: Key, mods: Mods) -> Self {
        Self { key, mods }
    }

    /// Build a plain (modifier-free) event.
    pub const fn plain(key: Key) -> Self {
        Self::new(key, Mods::NONE)
    }

    /// True when this event is Ctrl+C (the universal interrupt key).
    pub fn is_ctrl_c(&self) -> bool {
        self.mods.ctrl && self.key == Key::Char('c')
    }

    /// True when this event is Ctrl+D (EOF / delete-forward).
    pub fn is_ctrl_d(&self) -> bool {
        self.mods.ctrl && self.key == Key::Char('d')
    }

    /// True for a bare (unmodified) Enter.
    pub fn is_enter(&self) -> bool {
        *self == KeyEvent::plain(Key::Enter)
    }
}

impl From<crossterm::event::KeyEvent> for KeyEvent {
    fn from(event: crossterm::event::KeyEvent) -> Self {
        use crossterm::event::KeyCode;
        // Only key presses and repeats drive UI state; release events
        // (Kitty protocol) are dropped by the caller before conversion.
        let state = event.modifiers;
        let mods = Mods {
            ctrl: state.contains(crossterm::event::KeyModifiers::CONTROL),
            alt: state.contains(crossterm::event::KeyModifiers::ALT),
            shift: state.contains(crossterm::event::KeyModifiers::SHIFT),
        };
        let key = match event.code {
            KeyCode::Char(c) => Key::Char(c),
            KeyCode::Enter => Key::Enter,
            // Shift+Tab arrives as BackTab on most platforms, often
            // without the SHIFT modifier; normalize onto Tab+Shift.
            KeyCode::Tab | KeyCode::BackTab => Key::Tab,
            KeyCode::Backspace => Key::Backspace,
            KeyCode::Esc => Key::Esc,
            KeyCode::Left => Key::Left,
            KeyCode::Right => Key::Right,
            KeyCode::Up => Key::Up,
            KeyCode::Down => Key::Down,
            KeyCode::Home => Key::Home,
            KeyCode::End => Key::End,
            KeyCode::PageUp => Key::PageUp,
            KeyCode::PageDown => Key::PageDown,
            KeyCode::Insert => Key::Insert,
            KeyCode::Delete => Key::Delete,
            KeyCode::F(n) => Key::F(n),
            _ => Key::Other,
        };
        let shift = mods.shift || event.code == KeyCode::BackTab;
        Self {
            key,
            mods: Mods { shift, ..mods },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_c_and_d_detection() {
        assert!(KeyEvent::new(Key::Char('c'), Mods::CTRL).is_ctrl_c());
        assert!(!KeyEvent::plain(Key::Char('c')).is_ctrl_c());
        assert!(KeyEvent::new(Key::Char('d'), Mods::CTRL).is_ctrl_d());
    }

    #[test]
    fn backtab_normalizes_to_shift_tab() {
        let event = KeyEvent::from(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::BackTab,
            crossterm::event::KeyModifiers::empty(),
        ));
        assert_eq!(event.key, Key::Tab);
        assert!(event.mods.shift);
    }

    #[test]
    fn enter_recognized_only_unmodified() {
        assert!(KeyEvent::plain(Key::Enter).is_enter());
        assert!(!KeyEvent::new(Key::Enter, Mods::CTRL).is_enter());
    }
}
