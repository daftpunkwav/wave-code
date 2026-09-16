//! Keyboard and paste input: keys → state transitions; while the
//! approval popup is open every key routes to it (see [`super::approval`]).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use wavecode_wire::Op;

use super::App;
use crate::text::sanitize_terminal;

impl App {
    /// Keys → state transitions (every key routes to the popup while
    /// the approval popup is open).
    pub fn handle_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if self.approval.is_some() {
            self.handle_approval_key(key);
            return;
        }
        if self.question.is_some() {
            self.handle_question_key(key);
            return;
        }
        match key.code {
            KeyCode::Enter => {
                // Popup open and input differs from the selection:
                // complete first; otherwise submit.
                if self.slash_visible()
                    && self
                        .slash_candidates()
                        .get(self.slash_selected)
                        .is_some_and(|c| *c != self.input)
                {
                    self.complete_slash();
                } else {
                    self.submit_input();
                }
            }
            KeyCode::Esc => {
                if self.slash_visible() {
                    self.slash_dismissed = true;
                } else if self.in_turn {
                    self.outbox.push(Op::Interrupt);
                }
            }
            KeyCode::Tab => {
                if self.slash_visible() {
                    self.complete_slash();
                }
            }
            KeyCode::Up => {
                if self.slash_visible() {
                    let n = self.slash_candidates().len();
                    self.slash_selected = (self.slash_selected + n - 1) % n;
                }
            }
            KeyCode::Down => {
                if self.slash_visible() {
                    let n = self.slash_candidates().len();
                    self.slash_selected = (self.slash_selected + 1) % n;
                }
            }
            KeyCode::PageUp => self.scroll_by(-10),
            KeyCode::PageDown => self.scroll_by(10),
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    let byte = Self::char_to_byte(&self.input, self.cursor - 1);
                    self.input.remove(byte);
                    self.cursor -= 1;
                    self.on_input_changed();
                }
            }
            KeyCode::Delete => {
                if self.cursor < self.input.chars().count() {
                    let byte = Self::char_to_byte(&self.input, self.cursor);
                    self.input.remove(byte);
                    self.on_input_changed();
                }
            }
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
            }
            KeyCode::Right => {
                self.cursor = (self.cursor + 1).min(self.input.chars().count());
            }
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.chars().count(),
            KeyCode::Char(c) => {
                let byte = Self::char_to_byte(&self.input, self.cursor);
                self.input.insert(byte, c);
                self.cursor += 1;
                self.on_input_changed();
            }
            _ => {}
        }
    }

    /// Paste events: insert sanitized (bracketed paste guards against
    /// injection). Routing matches [`App::handle_key`] while the approval
    /// popup is open — every key goes to the popup: pastes enter the
    /// reason in reason mode and are ignored otherwise (the main input
    /// hides behind the popup, so silent writes would be invisible). The
    /// question popup takes pastes into its free-text answer for the
    /// same reason.
    pub fn paste(&mut self, s: &str) {
        if let Some(popup) = &mut self.approval {
            if popup.reason_mode {
                popup.reason.extend(sanitize_terminal(s).chars());
            }
            return;
        }
        if let Some(popup) = &mut self.question {
            popup.input.extend(sanitize_terminal(s).chars());
            return;
        }
        let clean = sanitize_terminal(s);
        let byte = Self::char_to_byte(&self.input, self.cursor);
        self.input.insert_str(byte, &clean);
        self.cursor += clean.chars().count();
        self.on_input_changed();
    }

    /// Mouse wheel: scroll the message stream.
    pub fn scroll_by(&mut self, delta: isize) {
        if delta < 0 {
            self.follow_tail = false;
            self.scroll = self.scroll.saturating_sub(delta.unsigned_abs());
        } else {
            self.scroll = self.scroll.saturating_add(delta as usize);
            // Bottom recovery is decided at draw time in ui (needs the
            // visible height).
        }
    }

    pub(super) fn on_input_changed(&mut self) {
        self.slash_dismissed = false;
        self.slash_selected = 0;
    }

    /// Character index → byte index (the cursor counts characters, so
    /// UTF-8 is never split).
    pub(super) fn char_to_byte(s: &str, char_idx: usize) -> usize {
        s.char_indices()
            .nth(char_idx)
            .map(|(b, _)| b)
            .unwrap_or(s.len())
    }
}
