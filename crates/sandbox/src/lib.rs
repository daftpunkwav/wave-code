//! wavecode-sandbox — 权限与执行安全层（SPEC §12，P2 落地策略层）。
//!
//! 纯逻辑、100% 可单测：
//! - [`PermissionMode`] 四档（protocol 线型）：default / plan / acceptEdits /
//!   bypassPermissions；
//! - allow / deny 规则解析与匹配：条目形如 `Bash(git *)`、`File(src/**)`，
//!   匹配顺序 **deny 优先**，命中 allow 免审批；
//! - [`Sandbox::decide`]：给定工具名 + 输入 + 工具属性（只读 / 破坏性）→
//!   [`Verdict::Allow`] / [`Verdict::Ask`] / [`Verdict::Deny`]。
//!
//! OS 级沙箱（Linux landlock / macOS seatbelt / Windows ACL）与权限模式正交
//!（机制与策略分离），为后续里程碑，见 docs/SPEC.md §17。

use std::sync::{Arc, Mutex};

use wavecode_protocol::{ApprovalKind, PermissionMode};

/// Ask 详情（ApprovalRequested.detail）的字符上限：命令全文可能很长，
/// 事件载荷须有限；前端渲染另有截断。
const DETAIL_MAX_CHARS: usize = 500;

/// 规则作用域（条目前缀）：`Bash(...)` 匹配命令全文，`File(...)` 匹配路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleScope {
    Bash,
    File,
}

impl RuleScope {
    fn name(&self) -> &'static str {
        match self {
            Self::Bash => "Bash",
            Self::File => "File",
        }
    }
}

/// 单条权限规则：作用域 + 通配模式（如 `Bash(git *)`、`File(src/**)`）。
///
/// 通配语义（自实现，零第三方依赖）：`*` 匹配任意字符序列（含 `/` 与空串，
/// 即不区分 `*` 与 `**`），`?` 匹配单个字符，其余字符字面匹配。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    scope: RuleScope,
    pattern: String,
}

/// 规则解析错误。
#[derive(Debug, thiserror::Error)]
pub enum RuleError {
    /// 条目形态非法：须为 `Scope(pattern)`，Scope ∈ {Bash, File}，pattern 非空。
    #[error("无效权限规则: {0}（期望形如 Bash(git *) 或 File(src/**)）")]
    Invalid(String),
}

impl Rule {
    /// 解析规则条目：`Scope(pattern)`，Scope ∈ {`Bash`, `File`}，pattern 非空。
    pub fn parse(entry: &str) -> Result<Self, RuleError> {
        let invalid = || RuleError::Invalid(entry.to_owned());
        let open = entry.find('(').ok_or_else(invalid)?;
        let pattern = entry
            .strip_suffix(')')
            .ok_or_else(invalid)?
            .get(open + 1..)
            .ok_or_else(invalid)?;
        let scope = match &entry[..open] {
            "Bash" => RuleScope::Bash,
            "File" => RuleScope::File,
            _ => return Err(invalid()),
        };
        if pattern.is_empty() {
            return Err(invalid());
        }
        Ok(Self {
            scope,
            pattern: pattern.to_owned(),
        })
    }

    /// 规则是否命中本次调用：按作用域从输入取候选文本
    ///（Bash ← `command`，File ← `path`），候选缺失即不命中。
    fn matches(&self, input: &serde_json::Value) -> bool {
        let key = match self.scope {
            RuleScope::Bash => "command",
            RuleScope::File => "path",
        };
        input
            .get(key)
            .and_then(serde_json::Value::as_str)
            .is_some_and(|candidate| wildcard_match(&self.pattern, candidate))
    }
}

impl std::fmt::Display for Rule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}({})", self.scope.name(), self.pattern)
    }
}

/// 通配匹配：`*` 任意字符序列（含 `/` 与空串），`?` 单字符，其余字面。
/// 迭代 + 星号回溯，O(n·m) 最坏，规则与候选均短，可接受。
pub fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut star_t) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            star_t = ti;
            pi += 1;
        } else if let Some(s) = star {
            // 失配回退：星号多吃一个字符再试。
            pi = s + 1;
            star_t += 1;
            ti = star_t;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// 审批判定结果（给定工具调用的处置）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// 放行（只读默认、规则豁免、acceptEdits 文件编辑、bypassPermissions）。
    Allow,
    /// 需人工审批：core 发 `ApprovalRequested` 并 park 等待回填。
    Ask { kind: ApprovalKind, detail: String },
    /// 拒绝：reason 以 is_error ToolResult 回灌模型（deny 规则 / plan 模式）。
    Deny { reason: String },
}

/// 会话级权限状态：权限模式 + allow / deny 规则表。
///
/// 模式经 `Arc<Mutex<..>>` 共享（§17.5 M3"sandbox 规则状态同步 Arc 化"的
/// 首步）：actor 在 turn 进行中也能力经 [`Sandbox::mode_handle`] 切换模式，
/// 下一次 `decide` 即生效；规则表 P2 为只读快照（"始终放行写规则"待接线）。
#[derive(Debug, Clone)]
pub struct Sandbox {
    mode: Arc<Mutex<PermissionMode>>,
    allow: Vec<Rule>,
    deny: Vec<Rule>,
}

impl Sandbox {
    /// 新建：解析 allow / deny 规则条目，任一条目非法即整体报错（启动期
    /// 失败显式化，不做静默跳过——§19 禁止静默降级）。
    pub fn new(mode: PermissionMode, allow: &[String], deny: &[String]) -> Result<Self, RuleError> {
        let parse_all = |entries: &[String]| {
            entries
                .iter()
                .map(|e| Rule::parse(e))
                .collect::<Result<Vec<_>, _>>()
        };
        Ok(Self {
            mode: Arc::new(Mutex::new(mode)),
            allow: parse_all(allow)?,
            deny: parse_all(deny)?,
        })
    }

    /// 空规则的快捷构造（测试与默认装配）。
    pub fn without_rules(mode: PermissionMode) -> Self {
        Self::new(mode, &[], &[]).expect("空规则表解析不会失败")
    }

    /// 当前权限模式。
    pub fn mode(&self) -> PermissionMode {
        *self.mode.lock().expect("mode 锁中毒即进程已有 panic")
    }

    /// 模式共享句柄：actor 经此在 turn 进行中切换模式（下一次 decide 生效）。
    pub fn mode_handle(&self) -> Arc<Mutex<PermissionMode>> {
        self.mode.clone()
    }

    /// 审批判定：deny 规则优先（任何模式不豁免）→ allow 规则豁免 →
    /// session 内状态工具豁免（P4，`todo_write` 各模式免审批）→ 模式默认策略。
    ///
    /// `tool` / `input` 用于规则匹配与 Ask 详情；`read_only` / `destructive`
    /// 来自 Tool trait（core 传入，sandbox 不反向依赖 tools）。
    pub fn decide(
        &self,
        tool: &str,
        input: &serde_json::Value,
        read_only: bool,
        destructive: bool,
    ) -> Verdict {
        // 1. deny 优先：显式禁令在任何模式下都生效（bypassPermissions 不豁免）。
        if let Some(rule) = self.deny.iter().find(|r| r.matches(input)) {
            return Verdict::Deny {
                reason: format!("denied by permission rule: {rule}"),
            };
        }
        // 2. allow 命中：免审批直接放行。
        if self.allow.iter().any(|r| r.matches(input)) {
            return Verdict::Allow;
        }
        // 2.5 session 内状态工具豁免（P4）：todo_write 只改会话内存清单，
        // 不改文件系统、不 spawn 进程——各模式免审批直接放行（对齐 deepagents
        // write_todos 自动放行；plan 模式下维护清单本就是规划行为）。仍受
        // 上方 deny 规则约束（显式禁令不豁免）。
        if is_session_state(tool) {
            return Verdict::Allow;
        }
        // 3. 模式默认策略。只读且非破坏性在 default/acceptEdits/bypass 下放行；
        // plan 模式只读才可用，其余直接拒绝回灌（不发审批请求）。
        match self.mode() {
            PermissionMode::Plan if !read_only || destructive => Verdict::Deny {
                reason: format!(
                    "plan mode: only read-only tools are allowed; `{tool}` was blocked (no changes were made)"
                ),
            },
            PermissionMode::BypassPermissions => Verdict::Allow,
            _ if read_only && !destructive => Verdict::Allow,
            // acceptEdits：文件编辑自动放行（shell 与破坏性工具仍审批）。
            PermissionMode::AcceptEdits if is_file_edit(tool) && !destructive => Verdict::Allow,
            _ => Verdict::Ask {
                kind: approval_kind(tool),
                detail: ask_detail(tool, input),
            },
        }
    }
}

/// acceptEdits 模式自动放行的文件编辑工具（内置集；MCP 写工具 P2 不在此列）。
fn is_file_edit(tool: &str) -> bool {
    matches!(tool, "write_file" | "edit_file")
}

/// session 内状态工具（P4）：只改会话内存状态、无外部副作用，各模式免审批。
fn is_session_state(tool: &str) -> bool {
    matches!(tool, "todo_write")
}

/// 审批类别：shell 命令为 Exec，其余（文件写 / 编辑等）为 Write。
fn approval_kind(tool: &str) -> ApprovalKind {
    if tool == "shell" {
        ApprovalKind::Exec
    } else {
        ApprovalKind::Write
    }
}

/// Ask 详情：人类可读的调用摘要（shell 带命令全文，文件工具带路径，
/// 其余回退紧凑 JSON），按字符截断。
fn ask_detail(tool: &str, input: &serde_json::Value) -> String {
    let target = input
        .get("command")
        .or_else(|| input.get("path"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| input.to_string());
    let detail = format!("{tool}: {target}");
    if detail.chars().count() <= DETAIL_MAX_CHARS {
        detail
    } else {
        let mut t: String = detail.chars().take(DETAIL_MAX_CHARS - 1).collect();
        t.push('…');
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // —— 规则解析 ——

    #[test]
    fn rule_parse_roundtrip() {
        let rule = Rule::parse("Bash(git *)").unwrap();
        assert_eq!(rule.scope, RuleScope::Bash);
        assert_eq!(rule.to_string(), "Bash(git *)");
        let rule = Rule::parse("File(src/**)").unwrap();
        assert_eq!(rule.scope, RuleScope::File);
        assert_eq!(rule.to_string(), "File(src/**)");
        // 模式内含括号：取首个 `(` 为分界，末位 `)` 为结尾。
        assert_eq!(Rule::parse("Bash(echo (hi))").unwrap().pattern, "echo (hi)");
    }

    #[test]
    fn rule_parse_rejects_malformed() {
        for bad in [
            "",
            "Bash",
            "Bash()",
            "Nope(x)",
            "bash(git *)", // 作用域大小写敏感，与 SPEC §12 示例一致
            "Bash(git *",
            "git *",
        ] {
            assert!(Rule::parse(bad).is_err(), "应拒绝: {bad:?}");
        }
    }

    // —— 通配匹配 ——

    #[test]
    fn wildcard_semantics() {
        assert!(wildcard_match("git *", "git status"));
        assert!(wildcard_match("git *", "git push origin main"));
        assert!(!wildcard_match("git *", "git")); // `*` 前的空格是字面量
        assert!(wildcard_match("src/**", "src/a/b.rs"));
        assert!(wildcard_match("src/*", "src/a/b.rs")); // `*` 可跨 `/`（与 `**` 同义）
        assert!(!wildcard_match("src/*", "other/a.rs"));
        assert!(wildcard_match("*.rs", "a/b.rs"));
        assert!(wildcard_match("?.rs", "a.rs"));
        assert!(!wildcard_match("?.rs", "ab.rs"));
        assert!(wildcard_match("npm run test", "npm run test"));
        assert!(!wildcard_match("npm run test", "npm run test --watch"));
        assert!(wildcard_match("*", "anything at all"));
        assert!(wildcard_match("", ""));
        assert!(!wildcard_match("", "x"));
    }

    // —— decide：规则优先级 ——

    fn shell_input(cmd: &str) -> serde_json::Value {
        json!({"command": cmd})
    }

    fn file_input(path: &str) -> serde_json::Value {
        json!({"path": path})
    }

    #[test]
    fn deny_rules_win_over_allow() {
        let sb = Sandbox::new(
            PermissionMode::Default,
            &["Bash(git *)".into()],
            &["Bash(git push *)".into()],
        )
        .unwrap();
        // 命中 allow 且未命中 deny：免审批放行
        assert_eq!(
            sb.decide("shell", &shell_input("git status"), false, false),
            Verdict::Allow
        );
        // 同时命中 allow 与 deny：deny 优先
        assert_eq!(
            sb.decide("shell", &shell_input("git push origin main"), false, false),
            Verdict::Deny {
                reason: "denied by permission rule: Bash(git push *)".into()
            }
        );
    }

    #[test]
    fn deny_rules_apply_even_in_bypass_mode() {
        let sb = Sandbox::new(
            PermissionMode::BypassPermissions,
            &[],
            &["File(secrets/**)".into()],
        )
        .unwrap();
        assert_eq!(
            sb.decide("read_file", &file_input("secrets/key.pem"), true, false),
            Verdict::Deny {
                reason: "denied by permission rule: File(secrets/**)".into()
            }
        );
        // 未命中 deny：bypass 全放行
        assert_eq!(
            sb.decide("shell", &shell_input("rm -rf build/"), false, true),
            Verdict::Allow
        );
    }

    #[test]
    fn file_rules_match_path_input() {
        let sb = Sandbox::new(
            PermissionMode::Default,
            &["File(src/**)".into()],
            &["File(src/secret.rs)".into()],
        )
        .unwrap();
        assert_eq!(
            sb.decide("write_file", &file_input("src/main.rs"), false, false),
            Verdict::Allow
        );
        assert!(matches!(
            sb.decide("write_file", &file_input("src/secret.rs"), false, false),
            Verdict::Deny { .. }
        ));
        // 未命中任何规则：default 模式非只读 → Ask
        assert!(matches!(
            sb.decide("write_file", &file_input("docs/x.md"), false, false),
            Verdict::Ask { .. }
        ));
    }

    #[test]
    fn invalid_rule_entry_is_startup_error() {
        assert!(Sandbox::new(PermissionMode::Default, &["Bash(".into()], &[]).is_err());
    }

    // —— decide：模式默认策略 ——

    #[test]
    fn default_mode_asks_for_writes_allows_reads() {
        let sb = Sandbox::without_rules(PermissionMode::Default);
        assert_eq!(
            sb.decide("read_file", &file_input("a.txt"), true, false),
            Verdict::Allow
        );
        assert_eq!(
            sb.decide("write_file", &file_input("a.txt"), false, false),
            Verdict::Ask {
                kind: ApprovalKind::Write,
                detail: "write_file: a.txt".into()
            }
        );
        assert_eq!(
            sb.decide("shell", &shell_input("cargo test"), false, false),
            Verdict::Ask {
                kind: ApprovalKind::Exec,
                detail: "shell: cargo test".into()
            }
        );
        // 破坏性工具即便只读标记为真也须审批
        assert!(matches!(
            sb.decide("shell", &shell_input("rm x"), true, true),
            Verdict::Ask { .. }
        ));
    }

    /// P4：session 内状态工具（todo_write）在 default / plan 模式下同样免审批
    ///（deny 规则判定仍在豁免之前——todo 输入无 command/path 候选键，实际
    /// 不会被规则命中，豁免不改变"deny 优先"的判定序）。
    #[test]
    fn session_state_tools_allowed_in_all_modes() {
        let input = json!({"todos": [{"content": "x", "status": "pending"}]});
        for mode in [
            PermissionMode::Default,
            PermissionMode::Plan,
            PermissionMode::AcceptEdits,
            PermissionMode::BypassPermissions,
        ] {
            let sb = Sandbox::without_rules(mode);
            assert_eq!(
                sb.decide("todo_write", &input, false, false),
                Verdict::Allow,
                "{mode:?} 模式应免审批"
            );
        }
    }

    #[test]
    fn plan_mode_denies_non_readonly_without_asking() {
        let sb = Sandbox::without_rules(PermissionMode::Plan);
        // 非只读直接 Deny（不是 Ask：plan 模式不发审批请求）
        let v = sb.decide("write_file", &file_input("a.txt"), false, false);
        let Verdict::Deny { reason } = v else {
            panic!("plan 模式写工具应 Deny: {v:?}")
        };
        assert!(reason.contains("plan mode"));
        // 只读放行
        assert_eq!(
            sb.decide("grep", &json!({"pattern": "x"}), true, false),
            Verdict::Allow
        );
    }

    #[test]
    fn accept_edits_allows_file_edits_but_asks_shell() {
        let sb = Sandbox::without_rules(PermissionMode::AcceptEdits);
        assert_eq!(
            sb.decide("write_file", &file_input("a.txt"), false, false),
            Verdict::Allow
        );
        assert_eq!(
            sb.decide("edit_file", &file_input("a.txt"), false, false),
            Verdict::Allow
        );
        // shell 仍审批；破坏性文件操作（标记 destructive）也仍审批
        assert!(matches!(
            sb.decide("shell", &shell_input("ls"), false, false),
            Verdict::Ask { .. }
        ));
        assert!(matches!(
            sb.decide("write_file", &file_input("a.txt"), false, true),
            Verdict::Ask { .. }
        ));
    }

    #[test]
    fn bypass_mode_allows_everything_not_denied() {
        let sb = Sandbox::without_rules(PermissionMode::BypassPermissions);
        assert_eq!(
            sb.decide("shell", &shell_input("rm -rf target"), false, true),
            Verdict::Allow
        );
    }

    #[test]
    fn mode_handle_switch_takes_effect_on_next_decide() {
        let sb = Sandbox::without_rules(PermissionMode::Default);
        let handle = sb.mode_handle();
        assert!(matches!(
            sb.decide("write_file", &file_input("a.txt"), false, false),
            Verdict::Ask { .. }
        ));
        *handle.lock().unwrap() = PermissionMode::Plan;
        assert!(matches!(
            sb.decide("write_file", &file_input("a.txt"), false, false),
            Verdict::Deny { .. }
        ));
        assert_eq!(sb.mode(), PermissionMode::Plan);
    }

    #[test]
    fn ask_detail_truncates_long_input() {
        let sb = Sandbox::without_rules(PermissionMode::Default);
        let long = "x".repeat(2000);
        let v = sb.decide("shell", &shell_input(&long), false, false);
        let Verdict::Ask { detail, .. } = v else {
            panic!("应 Ask: {v:?}")
        };
        assert!(detail.chars().count() <= DETAIL_MAX_CHARS);
        assert!(detail.ends_with('…'));
    }
}
