//! slash 命令面（自 app/mod.rs 拆分）：候选补全、路由与内置命令本地处置。

use wavecode_protocol::{Op, PermissionMode};

use super::App;
use super::types::{Item, dim, warn};
use crate::text::sanitize_terminal;

/// 内置 slash 命令（补全候选与路由共用；skill 名由装配侧注入）。
const BUILTIN_COMMANDS: &[&str] = &["compact", "memory", "mcp", "permissions", "quit", "exit"];

impl App {
    /// 当前输入派生的 slash 候选（内置命令 + 可直调 skill，按前缀过滤）。
    pub fn slash_candidates(&self) -> Vec<String> {
        let Some(prefix) = self.input.strip_prefix('/') else {
            return Vec::new();
        };
        if prefix.contains(char::is_whitespace) {
            return Vec::new();
        }
        BUILTIN_COMMANDS
            .iter()
            .filter(|name| name.starts_with(prefix))
            .map(|name| format!("/{name}"))
            .chain(
                self.ctx
                    .skill_names
                    .iter()
                    .filter(|name| name.starts_with(prefix))
                    .map(|name| format!("/{name}")),
            )
            .collect()
    }

    /// slash 弹层是否可见：`/` 起始、无参数空白、未被 Esc 关闭、有候选。
    pub fn slash_visible(&self) -> bool {
        !self.slash_dismissed && !self.slash_candidates().is_empty()
    }

    pub fn slash_selected(&self) -> usize {
        self.slash_selected
    }

    /// 将选中候选填入输入框。
    pub(super) fn complete_slash(&mut self) {
        let candidates = self.slash_candidates();
        let idx = self.slash_selected.min(candidates.len().saturating_sub(1));
        if let Some(c) = candidates.get(idx) {
            self.input = c.clone();
            self.cursor = self.input.chars().count();
        }
    }

    /// Enter 提交：入消息流（`> ` 前缀）并按 slash 路由产生 Op。
    pub(super) fn submit_input(&mut self) {
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.input.clear();
        self.cursor = 0;
        self.slash_dismissed = false;
        self.slash_selected = 0;
        self.items.push(Item::user(&text));
        match text.strip_prefix('/') {
            None => self.outbox.push(Op::UserInput { text }),
            Some(rest) => self.route_slash(rest),
        }
        self.follow_tail = true;
    }

    /// slash 路由：内置命令本地处置；其余按 skill 名查找后直调。
    fn route_slash(&mut self, rest: &str) {
        let (name, args) = match rest.split_once(char::is_whitespace) {
            Some((n, a)) => (n, a.trim()),
            None => (rest, ""),
        };
        match name {
            "quit" | "exit" => self.quit = true,
            "compact" => self.outbox.push(Op::Compact),
            // `/memory` 走协议面（Op::MemoryList → EventMsg::MemoryIndex）:
            // 前端不直读记忆文件,与 Web/Desktop 能力等价（SPEC §3 规则 2）。
            "memory" => self.outbox.push(Op::MemoryList),
            "mcp" => self.show_mcp(),
            "permissions" => self.cycle_permission_mode(),
            _ => {
                if self.ctx.skill_names.iter().any(|n| n == name) {
                    self.outbox.push(Op::SlashCommand {
                        name: name.to_owned(),
                        args: args.to_owned(),
                    });
                } else {
                    self.items.push(Item::plain(
                        format!(
                            "未知命令：/{name}（内置：{}；其余 / 前缀为 skill 直调）",
                            BUILTIN_COMMANDS
                                .iter()
                                .map(|c| format!("/{c}"))
                                .collect::<Vec<_>>()
                                .join(" ")
                        ),
                        warn(),
                    ));
                }
            }
        }
    }

    /// `/mcp`：展示已配置 server 状态行（core 预渲染，P9；首版状态恒为
    /// "未连接（transport 未实现）"——诚实展示，不伪造在线状态）。
    fn show_mcp(&mut self) {
        if self.ctx.mcp_server_lines.is_empty() {
            self.items.push(Item::plain(
                "（未配置 MCP server；在 config.toml 添加 [mcp_servers.<name>] 段）".into(),
                dim(),
            ));
        } else {
            for line in &self.ctx.mcp_server_lines {
                self.items
                    .push(Item::plain(sanitize_terminal(line).into_owned(), dim()));
            }
        }
    }

    /// `/permissions`：四档循环切换，Op 同步 core 侧、本地立即生效。
    fn cycle_permission_mode(&mut self) {
        let next = match self.permission_mode {
            PermissionMode::Default => PermissionMode::Plan,
            PermissionMode::Plan => PermissionMode::AcceptEdits,
            PermissionMode::AcceptEdits => PermissionMode::BypassPermissions,
            _ => PermissionMode::Default,
        };
        self.permission_mode = next;
        self.outbox.push(Op::SetPermissionMode { mode: next });
        self.items.push(Item::plain(
            format!("权限模式切换为 {next}（写 / 执行工具的审批策略随之变化）"),
            dim(),
        ));
    }
}
