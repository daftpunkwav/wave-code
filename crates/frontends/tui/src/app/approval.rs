//! Approval popup key routing: every key goes here while open.

use crossterm::event::{KeyCode, KeyEvent};
use wavecode_wire::{Op, WireDecision};

use super::App;

impl App {
    /// Approval popup keys: y allows; n enters reason mode (Enter
    /// confirms the denial); Esc denies at once (no parked hang).
    pub(super) fn handle_approval_key(&mut self, key: KeyEvent) {
        let Some(popup) = &mut self.approval else {
            return;
        };
        if popup.reason_mode {
            match key.code {
                KeyCode::Enter => {
                    let popup = self.approval.take().expect("checked Some above");
                    self.outbox.push(Op::ExecApproval {
                        call_id: popup.call_id,
                        decision: WireDecision::Deny {
                            reason: popup.reason,
                        },
                    });
                }
                KeyCode::Esc => {
                    let popup = self.approval.take().expect("checked Some above");
                    self.outbox.push(Op::ExecApproval {
                        call_id: popup.call_id,
                        decision: WireDecision::Deny {
                            reason: String::new(),
                        },
                    });
                }
                KeyCode::Backspace => {
                    popup.reason.pop();
                }
                KeyCode::Char(c) => popup.reason.push(c),
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                let popup = self.approval.take().expect("checked Some above");
                self.outbox.push(Op::ExecApproval {
                    call_id: popup.call_id,
                    decision: WireDecision::AllowOnce,
                });
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {
                popup.reason_mode = true;
            }
            KeyCode::Esc => {
                let popup = self.approval.take().expect("checked Some above");
                self.outbox.push(Op::ExecApproval {
                    call_id: popup.call_id,
                    decision: WireDecision::Deny {
                        reason: String::new(),
                    },
                });
            }
            _ => {}
        }
    }
}
