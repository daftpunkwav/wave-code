//! 审批弹窗按键路由（自 app/mod.rs 拆分）：弹窗打开时全部按键在此处置。

use crossterm::event::{KeyCode, KeyEvent};
use wavecode_protocol::{ApprovalDecision, Op};

use super::App;

impl App {
    /// 审批弹窗按键：y 放行；n 进入原因录入态（Enter 确认拒绝）；
    /// Esc 直接拒绝（不留 park 悬挂）。
    pub(super) fn handle_approval_key(&mut self, key: KeyEvent) {
        let Some(popup) = &mut self.approval else {
            return;
        };
        if popup.reason_mode {
            match key.code {
                KeyCode::Enter => {
                    let popup = self.approval.take().expect("上面已判定 Some");
                    self.outbox.push(Op::ExecApproval {
                        call_id: popup.call_id,
                        decision: ApprovalDecision::Deny {
                            reason: popup.reason,
                        },
                    });
                }
                KeyCode::Esc => {
                    let popup = self.approval.take().expect("上面已判定 Some");
                    self.outbox.push(Op::ExecApproval {
                        call_id: popup.call_id,
                        decision: ApprovalDecision::Deny {
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
                let popup = self.approval.take().expect("上面已判定 Some");
                self.outbox.push(Op::ExecApproval {
                    call_id: popup.call_id,
                    decision: ApprovalDecision::AllowOnce,
                });
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {
                popup.reason_mode = true;
            }
            KeyCode::Esc => {
                let popup = self.approval.take().expect("上面已判定 Some");
                self.outbox.push(Op::ExecApproval {
                    call_id: popup.call_id,
                    decision: ApprovalDecision::Deny {
                        reason: String::new(),
                    },
                });
            }
            _ => {}
        }
    }
}
