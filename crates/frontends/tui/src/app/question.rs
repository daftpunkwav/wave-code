//! Question popup key routing: every key goes here while open.

use crossterm::event::{KeyCode, KeyEvent};
use wavecode_wire::Op;

use super::App;

impl App {
    /// Question popup keys: 1-4 pick a numbered option; typed characters
    /// accumulate as a free-text answer (Enter submits it); Esc submits
    /// an empty answer (dismissed). Empty answers surface as "dismissed"
    /// in the tool result instead of failing.
    pub(super) fn handle_question_key(&mut self, key: KeyEvent) {
        let Some(popup) = &mut self.question else {
            return;
        };
        match key.code {
            // Number keys select a listed option in one keystroke; when
            // no option sits at that number the digit falls through into
            // the free-text answer (parity with the REPL prompt).
            KeyCode::Char(c @ '1'..='4') => {
                let index = (c as u8 - b'1') as usize;
                match popup.options.get(index).cloned() {
                    Some(option) => {
                        let popup = self.question.take().expect("checked Some above");
                        self.outbox.push(Op::QuestionAnswer {
                            call_id: popup.call_id,
                            answer: option,
                        });
                    }
                    None => popup.input.push(c),
                }
            }
            KeyCode::Enter => {
                let answer = popup.input.trim().to_string();
                let popup = self.question.take().expect("checked Some above");
                self.outbox.push(Op::QuestionAnswer {
                    call_id: popup.call_id,
                    answer,
                });
            }
            KeyCode::Esc => {
                let popup = self.question.take().expect("checked Some above");
                self.outbox.push(Op::QuestionAnswer {
                    call_id: popup.call_id,
                    answer: String::new(),
                });
            }
            KeyCode::Backspace => {
                popup.input.pop();
            }
            KeyCode::Char(c) => popup.input.push(c),
            _ => {}
        }
    }
}
