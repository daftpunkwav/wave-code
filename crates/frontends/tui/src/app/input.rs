//! 键盘与粘贴输入（自 app/mod.rs 拆分）：按键 → 状态迁移；审批弹窗打开
//! 时按键路由见 [`super::approval`]。

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use wavecode_protocol::Op;

use super::App;
use crate::text::sanitize_terminal;

impl App {
    /// 按键 → 状态迁移（审批弹窗打开时按键全部路由给弹窗）。
    pub fn handle_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if self.approval.is_some() {
            self.handle_approval_key(key);
            return;
        }
        match key.code {
            KeyCode::Enter => {
                // 弹层打开且输入不等于选中候选：先补全；否则提交。
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

    /// 粘贴事件：净化后插入（bracketed paste 防注入）。审批弹窗打开时与
    /// [`App::handle_key`] 同路由——按键全部交给弹窗：reason 录入态粘贴
    /// 进原因，其余状态忽略（主输入框被弹窗遮挡，静默写入不可见）。
    pub fn paste(&mut self, s: &str) {
        if let Some(popup) = &mut self.approval {
            if popup.reason_mode {
                popup.reason.extend(sanitize_terminal(s).chars());
            }
            return;
        }
        let clean = sanitize_terminal(s);
        let byte = Self::char_to_byte(&self.input, self.cursor);
        self.input.insert_str(byte, &clean);
        self.cursor += clean.chars().count();
        self.on_input_changed();
    }

    /// 鼠标滚轮：滚动消息流。
    pub fn scroll_by(&mut self, delta: isize) {
        if delta < 0 {
            self.follow_tail = false;
            self.scroll = self.scroll.saturating_sub(delta.unsigned_abs());
        } else {
            self.scroll = self.scroll.saturating_add(delta as usize);
            // 到达底部由 ui 绘制时判定并恢复 follow_tail（需要可视高度）。
        }
    }

    pub(super) fn on_input_changed(&mut self) {
        self.slash_dismissed = false;
        self.slash_selected = 0;
    }

    /// 字符索引 → 字节索引（cursor 按字符计，防切断 UTF-8）。
    pub(super) fn char_to_byte(s: &str, char_idx: usize) -> usize {
        s.char_indices()
            .nth(char_idx)
            .map(|(b, _)| b)
            .unwrap_or(s.len())
    }
}
