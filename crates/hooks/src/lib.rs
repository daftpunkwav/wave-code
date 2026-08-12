//! wavecode-hooks — 生命周期 hooks 系统（SPEC §9，P7 落地 command 类型）。
//!
//! 事件点：PreToolUse / PostToolUse / UserPromptSubmit / SessionStart /
//! SessionEnd / Stop / PreCompact / PostCompact（SPEC §9 表；Notification
//! 事件点本版不实现，留占位）。
//!
//! hook 类型：
//! - `command`（本版实现）：配置 `[hooks.<EventPoint>]` 表（或表数组），
//!   字段 matcher / command / timeout_ms / once；经平台 shell 执行
//!   （Windows `cmd /C`、Unix `sh -c`，`WAVECODE_SHELL` 可覆盖——与 shell
//!   工具同一启发式），事件载荷以 JSON 写 stdin；
//! - `prompt`（SPEC 定为 M4 后）：以模板调用模型裁决放行/阻塞，本版
//!   **不实现**，留占位（见 [`HookDef`] 注释）。
//!
//! 阻塞语义（SPEC §9）：退出码 0 放行；2 阻塞且 stderr 回传模型（仅
//! 可阻塞事件点：PreToolUse / UserPromptSubmit / Stop；其余事件点退出码 2
//! 降级为警告放行）；其他非零 = 警告放行；超时强制 kill 记 warning。
//!
//! 信任边界（与 shell 工具不同）：hook 命令来自用户自己的配置文件，
//! 属已授权配置而非模型产出，故不做环境变量剔除 / 路径约束。

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

/// 默认超时（SPEC §9 配置示例）：10s。
pub const DEFAULT_TIMEOUT_MS: u64 = 10_000;

/// 事件点（SPEC §9 表；config 的 `[hooks.<EventPoint>]` 表名与
/// [`HookEventPoint::parse`] 的合法值一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookEventPoint {
    /// 工具执行前（可阻塞）：命令审计、前置检查。
    PreToolUse,
    /// 工具执行后（不可阻塞）：lint / 格式化回写、通知。
    PostToolUse,
    /// 用户输入进入 turn 前（可阻塞）：注入额外上下文、敏感词拦截。
    UserPromptSubmit,
    /// 会话启动（不可阻塞）：环境初始化。
    SessionStart,
    /// 会话结束（不可阻塞）：清理。
    SessionEnd,
    /// turn 收尾前（可阻塞）：goal 模式未达成时阻止结束。
    Stop,
    /// 压缩前（不可阻塞）：留档。
    PreCompact,
    /// 压缩后（不可阻塞）：留档。
    PostCompact,
    // TODO(SPEC §9 表外)：Notification 事件点（系统通知转发）本版不实现。
}

impl HookEventPoint {
    /// 全部事件点（文档 / 校验用，固定序）。
    pub const ALL: [HookEventPoint; 8] = [
        Self::PreToolUse,
        Self::PostToolUse,
        Self::UserPromptSubmit,
        Self::SessionStart,
        Self::SessionEnd,
        Self::Stop,
        Self::PreCompact,
        Self::PostCompact,
    ];

    /// 解析事件点名（config `[hooks.<EventPoint>]` 表名）。
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str() == name)
    }

    /// 事件点名（配置表名 / 载荷与诊断文本共用）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::SessionStart => "SessionStart",
            Self::SessionEnd => "SessionEnd",
            Self::Stop => "Stop",
            Self::PreCompact => "PreCompact",
            Self::PostCompact => "PostCompact",
        }
    }

    /// 是否可阻塞（SPEC §9 表）：可阻塞事件点的退出码 2 阻塞并回传
    /// stderr；不可阻塞事件点的退出码 2 降级为警告放行。
    pub fn blockable(self) -> bool {
        matches!(self, Self::PreToolUse | Self::UserPromptSubmit | Self::Stop)
    }
}

/// 单条 hook 定义（`[hooks.<EventPoint>]` 表的一行）。
///
/// 首版仅 command 类型：本结构即 command hook；prompt 类型（SPEC §9，
/// 以模板调用模型裁决，M4 后）落地时预期改为带 tag 的枚举
/// （`type = "command" | "prompt"`），届时 config 线型同步演进。
#[derive(Debug, Clone)]
pub struct HookDef {
    /// 工具名匹配器（仅工具类事件点有意义）：`|` 分隔多值，`*` 匹配全部；
    /// None = 不过滤。非工具事件点配了 matcher 的条目永不触发。
    pub matcher: Option<String>,
    /// shell 命令串（经平台 shell 执行）。
    pub command: String,
    /// 超时（毫秒），超时强制 kill 记 warning。
    pub timeout_ms: u64,
    /// 每个会话只触发一次（触发指"实际执行"，matcher 未命中不消耗）。
    pub once: bool,
}

/// 一次 hook 触发的事件载荷。
#[derive(Debug, Clone, Copy)]
pub struct HookInput<'a> {
    /// hook 进程的工作目录（会话 cwd）。
    pub cwd: &'a Path,
    /// 工具名（PreToolUse / PostToolUse；其余事件点为 None）。
    pub tool_name: Option<&'a str>,
    /// 工具输入（PreToolUse / PostToolUse）。
    pub tool_input: Option<&'a serde_json::Value>,
    /// 工具输出摘要（PostToolUse；其余为 None）。
    pub tool_output: Option<&'a str>,
}

/// hook 执行的裁决。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum HookVerdict {
    /// 放行（含无 hook / 全部成功 / 仅警告）。
    #[default]
    Allow,
    /// 阻塞：负载为回传模型的 stderr（仅可阻塞事件点产生）。
    Block(String),
}

/// 一个事件点的执行报告：裁决 + 警告（非零退出码 / 超时 / spawn 失败）。
#[derive(Debug, Clone, Default)]
pub struct HookReport {
    /// 裁决（Allow / Block）。
    pub verdict: HookVerdict,
    /// 警告清单（调用方转 Warning 事件 / 日志）。
    pub warnings: Vec<String>,
}

/// hook 引擎：配置表 + once 触发记录。
///
/// once 的"每会话一次"以引擎实例为界——会话装配一个引擎（SessionConfig
/// 持有 `Arc<HookEngine>`），跨会话重建即重置。
pub struct HookEngine {
    defs: HashMap<HookEventPoint, Vec<HookDef>>,
    /// 已触发的 once 条目（事件点, 条目序号）。
    fired: Mutex<HashSet<(HookEventPoint, usize)>>,
}

impl HookEngine {
    /// 以事件点 → 条目表构造。
    pub fn new(defs: HashMap<HookEventPoint, Vec<HookDef>>) -> Self {
        Self {
            defs,
            fired: Mutex::new(HashSet::new()),
        }
    }

    /// 空引擎（任何事件点都无条目）——装配层据此省略挂接。
    pub fn is_empty(&self) -> bool {
        self.defs.values().all(Vec::is_empty)
    }

    /// 某事件点是否有条目（无条目的点短路，不构造载荷）。
    pub fn has_hooks(&self, point: HookEventPoint) -> bool {
        self.defs.get(&point).is_some_and(|d| !d.is_empty())
    }

    /// 触发一个事件点：按配置序逐条执行，汇总警告；可阻塞事件点的首个
    /// 退出码 2 短路返回 Block（后续条目不再执行——阻塞即终局）。
    pub async fn run(&self, point: HookEventPoint, input: &HookInput<'_>) -> HookReport {
        let mut report = HookReport::default();
        let Some(defs) = self.defs.get(&point) else {
            return report;
        };
        for (idx, def) in defs.iter().enumerate() {
            // matcher 过滤：仅工具类事件点可命中；非工具事件点配 matcher
            // 的条目永不触发（配置错误以警告形式浮现——见下）。
            if let Some(matcher) = &def.matcher {
                match input.tool_name {
                    Some(tool) if matcher_matches(matcher, tool) => {}
                    _ => continue,
                }
            }
            // once：matcher 命中后的"实际执行"才消耗额度。
            if def.once
                && !self
                    .fired
                    .lock()
                    .expect("once 锁中毒即进程已有 panic")
                    .insert((point, idx))
            {
                continue;
            }
            match run_command(point, def, input).await {
                ExecOutcome::Success => {}
                ExecOutcome::Blocked(stderr) => {
                    if point.blockable() {
                        report.verdict = HookVerdict::Block(stderr);
                        return report; // 首个阻塞即短路（阻塞即终局）
                    }
                    report.warnings.push(format!(
                        "[{}] hook `{}` 退出码 2，但该事件点不可阻塞（按警告放行）",
                        point.as_str(),
                        def.command
                    ));
                }
                ExecOutcome::NonZero(code, stderr) => {
                    report.warnings.push(format!(
                        "[{}] hook `{}` 退出码 {code}（警告放行）: {}",
                        point.as_str(),
                        def.command,
                        one_line(&stderr)
                    ));
                }
                ExecOutcome::Timeout => {
                    report.warnings.push(format!(
                        "[{}] hook `{}` 超时（{}ms）已强制终止（kill，按警告放行）",
                        point.as_str(),
                        def.command,
                        def.timeout_ms
                    ));
                }
                ExecOutcome::SpawnFailed(reason) => {
                    report.warnings.push(format!(
                        "[{}] hook `{}` 启动失败（按警告放行）: {reason}",
                        point.as_str(),
                        def.command
                    ));
                }
            }
        }
        report
    }
}

/// matcher 匹配：`|` 分隔多值（任一命中），`*` 匹配全部，其余精确相等。
fn matcher_matches(matcher: &str, tool: &str) -> bool {
    matcher
        .split('|')
        .map(str::trim)
        .any(|alt| alt == "*" || alt == tool)
}

/// 单次执行的结果（内部）。
enum ExecOutcome {
    /// 退出码 0。
    Success,
    /// 退出码 2：负载 stderr（回传模型的阻塞原因）。
    Blocked(String),
    /// 其他非零退出码：负载 stderr 摘要。
    NonZero(i32, String),
    /// 超时已 kill。
    Timeout,
    /// spawn 失败（命令不存在等）。
    SpawnFailed(String),
}

/// 选定 shell 程序与"执行命令串"参数（与 tools 的 shell 工具同一启发式）：
/// Windows `cmd /C`、Unix `sh -c`；`WAVECODE_SHELL` 覆盖（值含 `cmd` 按
/// `/C` 处理，否则按 `-c`）。
fn shell_invocation() -> (String, &'static str) {
    if let Ok(custom) = std::env::var("WAVECODE_SHELL") {
        if custom.to_lowercase().contains("cmd") {
            return (custom, "/C");
        }
        return (custom, "-c");
    }
    if cfg!(windows) {
        ("cmd".to_owned(), "/C")
    } else {
        ("sh".to_owned(), "-c")
    }
}

/// 执行一条 command hook：平台 shell + stdin 载荷 JSON + 超时 kill。
///
/// 超时 kill 依赖 `kill_on_drop`：超时分支 drop 掉 `wait_with_output`
/// future 即 drop child，tokio 发 kill；孙进程回收的已知限制与 shell
/// 工具相同（进程组级回收待 M2，见 tools/shell_tool.rs 注释）。
async fn run_command(point: HookEventPoint, def: &HookDef, input: &HookInput<'_>) -> ExecOutcome {
    let payload = serde_json::json!({
        "event": point.as_str(),
        "tool": input.tool_name,
        "input": input.tool_input,
        "output": input.tool_output,
    });
    let (prog, flag) = shell_invocation();
    let mut child = match tokio::process::Command::new(prog)
        .arg(flag)
        .arg(&def.command)
        .current_dir(input.cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return ExecOutcome::SpawnFailed(e.to_string()),
    };
    // stdin 写载荷后关闭（hook 读 stdin 的场景能见到 EOF）；写失败
    //（命令不读 stdin 提前退出）不算错误，继续等退出码。
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        let _ = stdin.write_all(payload.to_string().as_bytes()).await;
        let _ = stdin.shutdown().await;
    }
    let waited = tokio::time::timeout(
        Duration::from_millis(def.timeout_ms),
        child.wait_with_output(),
    )
    .await;
    match waited {
        Err(_) => ExecOutcome::Timeout, // future drop → kill_on_drop 杀进程
        Ok(Err(e)) => ExecOutcome::SpawnFailed(e.to_string()),
        Ok(Ok(output)) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            match output.status.code() {
                Some(0) => ExecOutcome::Success,
                Some(2) => ExecOutcome::Blocked(stderr),
                Some(code) => ExecOutcome::NonZero(code, stderr),
                // 被信号终止（无退出码）：按非零警告放行。
                None => ExecOutcome::NonZero(-1, stderr),
            }
        }
    }
}

/// 警告文本的单行化（stderr 可能多行，取首行防刷屏）。
fn one_line(text: &str) -> String {
    text.lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(200)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input<'a>(tool: Option<&'a str>) -> HookInput<'a> {
        HookInput {
            cwd: Path::new("."),
            tool_name: tool,
            tool_input: None,
            tool_output: None,
        }
    }

    fn engine(entries: &[(HookEventPoint, HookDef)]) -> HookEngine {
        let mut defs: HashMap<HookEventPoint, Vec<HookDef>> = HashMap::new();
        for (point, def) in entries {
            defs.entry(*point).or_default().push(def.clone());
        }
        HookEngine::new(defs)
    }

    fn def(command: &str) -> HookDef {
        HookDef {
            matcher: None,
            command: command.to_owned(),
            timeout_ms: DEFAULT_TIMEOUT_MS,
            once: false,
        }
    }

    /// 平台无关的命令构造：cmd 与 sh 都认 `exit N`；stderr 输出需要
    /// 分平台写法（cmd 用 `1>&2`，sh 同形但连接符不同）。
    fn exit_cmd(code: u32, stderr: &str) -> String {
        if stderr.is_empty() {
            format!("exit {code}")
        } else if cfg!(windows) {
            format!("echo {stderr} 1>&2 & exit {code}")
        } else {
            format!("echo {stderr} 1>&2; exit {code}")
        }
    }

    /// 超时测试的"睡眠"命令（cmd 无 sleep，用 ping 占位）。
    fn sleep_cmd() -> String {
        if cfg!(windows) {
            "ping -n 10 127.0.0.1 >nul".to_owned()
        } else {
            "sleep 10".to_owned()
        }
    }

    // —— matcher ——

    /// SPEC §9 验收：matcher 匹配（精确 / 多值 / 通配 / 未命中跳过）。
    #[test]
    fn matcher_semantics() {
        assert!(matcher_matches("shell", "shell"));
        assert!(matcher_matches("shell | write_file", "write_file"));
        assert!(matcher_matches("*", "anything"));
        assert!(!matcher_matches("shell", "write_file"));
        assert!(matcher_matches(" shell ", "shell")); // 分隔值前后空格 trim 后命中
    }

    #[tokio::test]
    async fn matcher_skips_non_matching_tool() {
        let e = engine(&[(
            HookEventPoint::PreToolUse,
            HookDef {
                matcher: Some("shell".to_owned()),
                ..def(&exit_cmd(2, "blocked-stderr"))
            },
        )]);
        // 工具名不匹配：hook 不执行，无阻塞无警告。
        let report = e
            .run(HookEventPoint::PreToolUse, &input(Some("write_file")))
            .await;
        assert_eq!(report.verdict, HookVerdict::Allow);
        assert!(report.warnings.is_empty());
        // 匹配：阻塞。
        let report = e
            .run(HookEventPoint::PreToolUse, &input(Some("shell")))
            .await;
        assert_eq!(
            report.verdict,
            HookVerdict::Block("blocked-stderr".to_owned())
        );
    }

    // —— 阻塞语义 ——

    /// SPEC §9 验收：退出码 0 放行；2 阻塞且 stderr 进入 Block 负载；
    /// 其他非零警告放行。
    #[tokio::test]
    async fn exit_code_semantics() {
        let ok = engine(&[(HookEventPoint::PreToolUse, def(&exit_cmd(0, "")))]);
        let report = ok
            .run(HookEventPoint::PreToolUse, &input(Some("shell")))
            .await;
        assert_eq!(report.verdict, HookVerdict::Allow);
        assert!(report.warnings.is_empty());

        // stderr 断言用 ASCII：Windows cmd 按 GBK 输出非 ASCII 字节，
        // UTF-8 有损解码会替换（非 ASCII 阻塞原因的保真受平台代码页限制）。
        let blocker = engine(&[(
            HookEventPoint::PreToolUse,
            def(&exit_cmd(2, "no-writes-today")),
        )]);
        let report = blocker
            .run(HookEventPoint::PreToolUse, &input(Some("shell")))
            .await;
        assert_eq!(
            report.verdict,
            HookVerdict::Block("no-writes-today".to_owned())
        );

        let failing = engine(&[(HookEventPoint::PreToolUse, def(&exit_cmd(1, "oops")))]);
        let report = failing
            .run(HookEventPoint::PreToolUse, &input(Some("shell")))
            .await;
        assert_eq!(report.verdict, HookVerdict::Allow, "退出码 1 警告放行");
        assert_eq!(report.warnings.len(), 1);
        assert!(report.warnings[0].contains("退出码 1"));
        assert!(report.warnings[0].contains("oops"));
    }

    /// 不可阻塞事件点的退出码 2：降级为警告放行（SPEC §9 表"可阻塞"列）。
    #[tokio::test]
    async fn exit_2_on_non_blockable_point_degrades_to_warning() {
        let e = engine(&[(HookEventPoint::PostToolUse, def(&exit_cmd(2, "ignored")))]);
        let report = e
            .run(HookEventPoint::PostToolUse, &input(Some("shell")))
            .await;
        assert_eq!(report.verdict, HookVerdict::Allow);
        assert_eq!(report.warnings.len(), 1);
        assert!(report.warnings[0].contains("不可阻塞"));
    }

    /// 多条 hook 顺序执行；可阻塞事件点首个退出码 2 短路后续条目。
    #[tokio::test]
    async fn first_block_short_circuits() {
        let e = engine(&[
            (HookEventPoint::Stop, def(&exit_cmd(2, "first"))),
            (HookEventPoint::Stop, def(&exit_cmd(1, "unreached"))),
        ]);
        let report = e.run(HookEventPoint::Stop, &input(None)).await;
        assert_eq!(report.verdict, HookVerdict::Block("first".to_owned()));
        assert!(
            report.warnings.is_empty(),
            "后续条目不执行: {:?}",
            report.warnings
        );
    }

    // —— 超时 ——

    /// SPEC §9 验收：超时强制 kill 记 warning（kill_on_drop 杀进程）。
    #[tokio::test]
    async fn timeout_kills_and_warns() {
        let e = engine(&[(
            HookEventPoint::PreToolUse,
            HookDef {
                timeout_ms: 200,
                ..def(&sleep_cmd())
            },
        )]);
        let start = std::time::Instant::now();
        let report = e
            .run(HookEventPoint::PreToolUse, &input(Some("shell")))
            .await;
        assert_eq!(report.verdict, HookVerdict::Allow);
        assert_eq!(report.warnings.len(), 1);
        assert!(report.warnings[0].contains("超时"));
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "超时应立即返回而不是等命令跑完: {:?}",
            start.elapsed()
        );
    }

    // —— once ——

    /// once：同一会话（引擎实例）只执行一次；matcher 未命中不消耗额度。
    #[tokio::test]
    async fn once_fires_only_first_time() {
        let e = engine(&[(
            HookEventPoint::PreToolUse,
            HookDef {
                matcher: Some("shell".to_owned()),
                once: true,
                ..def(&exit_cmd(2, "once-block"))
            },
        )]);
        // matcher 未命中：不消耗 once。
        let r = e
            .run(HookEventPoint::PreToolUse, &input(Some("grep")))
            .await;
        assert_eq!(r.verdict, HookVerdict::Allow);
        // 第一次命中：阻塞。
        let r = e
            .run(HookEventPoint::PreToolUse, &input(Some("shell")))
            .await;
        assert_eq!(r.verdict, HookVerdict::Block("once-block".to_owned()));
        // 第二次：once 已消耗，不再执行。
        let r = e
            .run(HookEventPoint::PreToolUse, &input(Some("shell")))
            .await;
        assert_eq!(r.verdict, HookVerdict::Allow);
        assert!(r.warnings.is_empty());
    }

    /// 事件点解析：合法名一一对应，非法名 None。
    #[test]
    fn event_point_parse_roundtrip() {
        for point in HookEventPoint::ALL {
            assert_eq!(HookEventPoint::parse(point.as_str()), Some(point));
        }
        assert_eq!(HookEventPoint::parse("pre_tool_use"), None);
        assert_eq!(HookEventPoint::parse("Notification"), None);
    }
}
