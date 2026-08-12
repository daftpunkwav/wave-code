//! wavecode-cli — 单二进制入口（M1）。
//!
//! M1 命令面：
//! - `wavecode`（无子命令）：TTY 下进入 ratatui TUI（P8）；非 TTY 或
//!   `--repl` 时回退行式 REPL（rustyline，流式渲染，降级/管道路径）；
//! - `wavecode exec "<prompt>"`：非交互单 turn；`--json` 时 stdout 输出
//!   JSONL（每行一个 Event），人类可读渲染转 stderr；
//! - `wavecode resume [thread-id]`（P10，SPEC §16）：无 id 时列出最近会话
//!   （rollout 文件 mtime 倒序 + 首条用户消息摘要），有 id 时 replay
//!   恢复后进入交互界面（TTY 进 TUI，否则行式 REPL，同默认命令）；
//! - `wavecode --model <name>` / `wavecode --config <path>`：覆盖 config 项。
//!
//! app-server / mcp / login 等子命令随后续里程碑落地。

mod bootstrap;
mod markdown;
mod render;
mod wave;

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use wavecode_app_server::InProcessClient;
use wavecode_core::SessionConfig;
use wavecode_protocol::{ApprovalDecision, EventMsg, Op, StopReason, Submission};

use crate::bootstrap::BootError;
use crate::render::HumanRenderer;

/// REPL 提示符：亮青波形符（rustyline 对含 ANSI 的 prompt 宽度计算经
/// 冒烟验证正常；若错位则退回同形无色版本，见 T6 冒烟）
const PROMPT: &str = "\x1b[96m∿\x1b[0m ";

#[derive(Parser)]
#[command(name = "wavecode", version, about = "WaveCode — AI coding agent")]
struct Cli {
    /// 覆盖 config.model
    #[arg(long, global = true)]
    model: Option<String>,
    /// 配置文件路径（默认 ~/.wavecode/config.toml）
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// 强制行式 REPL（默认 TTY 下进入 TUI；非 TTY 自动回退行式）
    #[arg(long, global = true)]
    repl: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// 非交互执行单个 prompt
    Exec {
        prompt: String,
        /// 以 JSONL 输出事件流（人类渲染转 stderr）
        #[arg(long)]
        json: bool,
    },
    /// 恢复历史会话（缺省 thread-id 时列出最近会话）
    Resume {
        /// 会话 thread-id（`wavecode resume` 列表可见）
        thread_id: Option<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<ExitCode> {
    let cli = Cli::parse();
    init_tracing();

    let boot = match bootstrap::load_session_config(cli.config.as_deref(), cli.model.as_deref()) {
        Ok(boot) => boot,
        Err(BootError::Config(e)) => {
            // 配置缺失 / 加载失败：中文指引 + 退出码 2。
            print_config_error(&e);
            return Ok(ExitCode::from(2));
        }
        Err(e) => return Err(e.into()),
    };
    // P9：`/mcp` 展示面——server 状态行经 core 渲染（REPL 与 TUI 共用；
    // 首版状态恒为"未连接（transport 未实现）"，诚实展示不伪造在线）。
    let mcp_lines: Vec<String> = boot
        .mcp_servers
        .iter()
        .map(wavecode_core::mcp::server_status_line)
        .collect();
    let cfg = boot.session;

    match cli.command {
        Some(Command::Exec { prompt, json }) => run_exec(cfg, &prompt, json).await,
        // P10：会话恢复（SPEC §16）——列表 / replay 恢复后进交互界面。
        Some(Command::Resume { thread_id }) => run_resume(cfg, mcp_lines, thread_id, cli.repl).await,
        // P8：TTY 下默认进入 ratatui TUI；非 TTY（管道/重定向）或 --repl
        // 回退行式 REPL（降级路径，渲染语义不变）。
        None if cli.repl || !std::io::stdout().is_terminal() => run_repl(cfg, mcp_lines).await,
        None => run_tui(cfg, mcp_lines).await,
    }
}

/// P10：`wavecode resume [thread-id]`（SPEC §16）。无 id 时列出最近会话
///（rollout 文件 mtime 倒序 + 首条用户消息摘要；SQLite 索引的首版降级
/// 形态，见 core::rollout 模块注释）；有 id 时校验并检查 rollout 存在，
/// 覆盖 boot 分配的 thread id 后进入交互界面（构造即 replay 恢复）。
async fn run_resume(
    mut cfg: SessionConfig,
    mcp_lines: Vec<String>,
    thread_id: Option<String>,
    force_repl: bool,
) -> anyhow::Result<ExitCode> {
    let Some(home) = wavecode_core::memory::home_dir() else {
        eprintln!("错误：无法解析用户主目录（USERPROFILE/HOME），会话持久化不可用");
        return Ok(ExitCode::from(2));
    };
    let root = wavecode_core::rollout::default_root(&home);
    let Some(id) = thread_id else {
        let threads = wavecode_core::rollout::list_threads(&root)?;
        if threads.is_empty() {
            println!("（暂无历史会话；目录：{}）", root.display());
            return Ok(ExitCode::SUCCESS);
        }
        println!("最近会话（按更新时间倒序）：");
        for t in &threads {
            let summary = t.first_user_text.as_deref().unwrap_or("（无用户消息）");
            println!(
                "  {}  {}  {} 条消息 / {} 次压缩  {}",
                t.thread_id,
                format_age(t.modified),
                t.message_count,
                t.compaction_count,
                summary
            );
        }
        println!("\n恢复会话：`wavecode resume <thread-id>`");
        return Ok(ExitCode::SUCCESS);
    };
    if !wavecode_core::rollout::is_valid_thread_id(&id) {
        eprintln!("错误：非法 thread-id {id:?}（仅允许字母数字 / - / _）");
        return Ok(ExitCode::from(2));
    }
    let path = wavecode_core::rollout::rollout_path(&root, &id)?;
    if !path.exists() {
        eprintln!(
            "错误：找不到会话 {id}（{}）；`wavecode resume` 可列出最近会话",
            path.display()
        );
        return Ok(ExitCode::from(2));
    }
    // 恢复信息行（Session 构造会再 replay 一次；rollout 文件为小文件，
    // 双读可接受）。
    let load = wavecode_core::rollout::load_rollout(&path)?;
    let restored = wavecode_core::rollout::replay(&load.records);
    let compactions = load
        .records
        .iter()
        .filter(|r| matches!(r, wavecode_core::rollout::RolloutRecord::Compaction { .. }))
        .count();
    println!(
        "已恢复会话 {id}（{} 条消息 / {compactions} 次压缩）",
        restored.len()
    );
    cfg.rollout = Some(wavecode_core::rollout::RolloutConfig {
        root,
        thread_id: id,
    });
    // 与默认命令同纪律：TTY 进 TUI，非 TTY 或 --repl 回退行式 REPL。
    if force_repl || !std::io::stdout().is_terminal() {
        run_repl(cfg, mcp_lines).await
    } else {
        run_tui(cfg, mcp_lines).await
    }
}

/// 列表的相对时间显示（"N 秒/分钟/小时/天前"；不引入日期库）。
fn format_age(modified: std::time::SystemTime) -> String {
    let Ok(age) = modified.elapsed() else {
        return "（时间未知）".to_owned();
    };
    let secs = age.as_secs();
    if secs < 60 {
        format!("{secs} 秒前")
    } else if secs < 3600 {
        format!("{} 分钟前", secs / 60)
    } else if secs < 86400 {
        format!("{} 小时前", secs / 3600)
    } else {
        format!("{} 天前", secs / 86400)
    }
}

/// P7：生命周期 hook（SessionStart / SessionEnd，不可阻塞，SPEC §9）——
/// 警告走 stderr（exec --json 的人类渲染面也是 stderr，不污染 JSONL）。
async fn run_lifecycle_hooks(
    hooks: &std::sync::Arc<wavecode_core::hooks::HookEngine>,
    point: wavecode_core::hooks::HookEventPoint,
    cwd: &std::path::Path,
) {
    let report = hooks
        .run(
            point,
            &wavecode_core::hooks::HookInput {
                cwd,
                tool_name: None,
                tool_input: None,
                tool_output: None,
            },
        )
        .await;
    for warning in &report.warnings {
        eprintln!("警告：{warning}");
    }
}

/// 日志初始化：走 stderr（stdout 留给 JSONL / 渲染输出），默认级别 off
/// （用户侧错误经事件流呈现），`RUST_LOG` 可开（兼容 T10 的 `RUST_LOG=off`）。
fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("off"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

/// 打印配置错误（中文，走 stderr）；文件缺失时附创建指引与模板。
fn print_config_error(err: &wavecode_config::ConfigError) {
    eprintln!("错误：{err}");
    if let wavecode_config::ConfigError::NotFound(path) = err {
        eprintln!(
            r#"
请创建配置文件 {}，内容示例：

model = "claude-sonnet-4-5"
model_provider = "anthropic"

[model_providers.anthropic]
type = "anthropic"
base_url = "https://api.anthropic.com"
# api key 二选一（env_key 优先）：
# 方式一（推荐）：env_key 指向环境变量名，运行时从该变量读取 key
env_key = "ANTHROPIC_API_KEY"
# 方式二：内联 api_key（注意保密，勿提交版本库）
# api_key = "sk-ant-..."
"#,
            path.display()
        );
    }
}

/// `exec`：非交互单 turn。退出码：Completed=0，其余 stop_reason / 事件流
/// 意外结束=1。
async fn run_exec(cfg: SessionConfig, prompt: &str, json: bool) -> anyhow::Result<ExitCode> {
    // P7：SessionStart hook（环境初始化；警告走 stderr）。
    if let Some(hooks) = &cfg.hooks {
        run_lifecycle_hooks(
            hooks,
            wavecode_core::hooks::HookEventPoint::SessionStart,
            &cfg.cwd,
        )
        .await;
    }
    let hooks = cfg.hooks.clone();
    let cwd = cfg.cwd.clone();
    let mut client = InProcessClient::spawn(cfg);
    // --json：stdout 只写 JSONL，人类渲染转 stderr。anstream 按 TTY 自动
    // 去色；等待动画仅 human 模式 && TTY 开启。
    let is_tty = std::io::stdout().is_terminal();
    let out: Box<dyn Write> = if json {
        Box::new(anstream::stderr())
    } else {
        Box::new(anstream::stdout())
    };
    let mut renderer = HumanRenderer::new(out, !json && is_tty);

    client
        .submit(new_submission(Op::UserInput {
            text: prompt.to_string(),
        }))
        .await?;
    // exec 是一次性非交互命令：无法就审批等待用户输入，显式自动拒绝并
    // 把原因回灌模型（诚实行为，不做静默放行）；REPL 才有内联问答。
    let mut approval = ApprovalHandling::AutoDeny;
    let outcome = consume_turn(&mut client, &mut renderer, json, &mut approval).await?;

    // 通知 actor 优雅关闭（不阻塞等待）；client 析构另有 abort 兜底。
    let _ = client.submit(new_submission(Op::Shutdown)).await;

    // P7：SessionEnd hook（清理；记忆自动提取由 actor Shutdown 路径触发）。
    if let Some(hooks) = &hooks {
        run_lifecycle_hooks(
            hooks,
            wavecode_core::hooks::HookEventPoint::SessionEnd,
            &cwd,
        )
        .await;
    }

    Ok(match outcome {
        // 断管（下游如 `| head` 提前关闭）：用户已取走所需输出，干净退出。
        ConsumeOutcome::TurnCompleted(StopReason::Completed) | ConsumeOutcome::BrokenPipe => {
            ExitCode::SUCCESS
        }
        _ => ExitCode::FAILURE,
    })
}

/// P8：ratatui TUI 入口（TTY 默认路径）。装配知识（模型名 / cwd / 初始
/// 权限模式 / 记忆索引路径 / 可直调 skill 清单）在此从 SessionConfig 提取
/// 为 [`wavecode_tui::TuiContext`]——tui 不能依赖 core，凡 core 拥有的
/// 知识都经该结构注入；会话驱动完全走 InProcessClient 协议面。
async fn run_tui(cfg: SessionConfig, mcp_lines: Vec<String>) -> anyhow::Result<ExitCode> {
    // P7：SessionStart hook（环境初始化；警告走 stderr——进入交替屏幕前
    // 打印，不污染 TUI 画面）。
    if let Some(hooks) = &cfg.hooks {
        run_lifecycle_hooks(
            hooks,
            wavecode_core::hooks::HookEventPoint::SessionStart,
            &cfg.cwd,
        )
        .await;
    }
    let hooks = cfg.hooks.clone();
    let cwd = cfg.cwd.clone();
    let ctx = wavecode_tui::TuiContext {
        model_name: cfg.model_name.clone(),
        cwd: cfg.cwd.clone(),
        permission_mode: cfg.sandbox.mode(),
        // `/memory` 读取面：与行式 REPL 同路径（store_root/索引文件名）。
        memory_index_path: cfg
            .memory
            .as_ref()
            .map(|m| m.store_root.join(wavecode_core::memory::INDEX_FILE)),
        // slash 补全与路由：仅 user-invocable skill 可直调（与 REPL 同判定）。
        skill_names: cfg
            .skills
            .as_ref()
            .map(|s| {
                s.set
                    .iter()
                    .filter(|skill| skill.meta.user_invocable)
                    .map(|skill| skill.name.clone())
                    .collect()
            })
            .unwrap_or_default(),
        // P9：`/mcp` 展示面（core 预渲染的状态行；tui 不依赖 core，
        // 经本字段注入，同 memory_index_path 纪律）。
        mcp_server_lines: mcp_lines,
    };
    let client = InProcessClient::spawn(cfg);
    wavecode_tui::run(client, ctx).await?;

    // P7：SessionEnd hook（清理；记忆自动提取由 tui 退出时的 Shutdown
    // 路径触发，与 run_repl 同机制）。
    if let Some(hooks) = &hooks {
        run_lifecycle_hooks(
            hooks,
            wavecode_core::hooks::HookEventPoint::SessionEnd,
            &cwd,
        )
        .await;
    }
    Ok(ExitCode::SUCCESS)
}

/// 基础交互 REPL：`/quit`、`/exit` 退出；空行跳过；其余输入作为 UserInput
/// 提交，流式渲染至 TurnCompleted 后回到提示符。Ctrl-D 退出，Ctrl-C 放弃
/// 当前输入行。
async fn run_repl(cfg: SessionConfig, mcp_lines: Vec<String>) -> anyhow::Result<ExitCode> {
    // 启动横幅：TTY 时先播放 12 帧滚动动画（80ms/帧），定格后打印横幅；
    // 非 TTY（管道）直接静态横幅（anstream 自动去色）。在 spawn 前打印，
    // cfg 尚未移动，可直接借用字段。
    let version = env!("CARGO_PKG_VERSION");
    let is_tty = std::io::stdout().is_terminal();
    let mut phase = 0.0f32;
    if is_tty {
        let mut out = anstream::stdout();
        for _ in 0..12 {
            phase += 0.35;
            write!(out, "\r{}", wave::frame(7, phase))?;
            out.flush()?;
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        }
    }
    anstream::print!(
        "{}",
        wave::banner(&cfg.model_name, &cfg.cwd, version, phase)
    );

    // P6：`/memory` 命令的读取面（存储根在 cfg 移入 client 前取出）。
    let memory_root = cfg.memory.as_ref().map(|m| m.store_root.clone());
    // P7：`/name [args]` slash 直调的查找面与生命周期 hook（cfg 移入前取出）。
    let skill_set = cfg.skills.as_ref().map(|s| s.set.clone());
    let hooks = cfg.hooks.clone();
    let cwd = cfg.cwd.clone();
    // P7：SessionStart hook（环境初始化；横幅之后、首个 prompt 之前）。
    if let Some(hooks) = &hooks {
        run_lifecycle_hooks(
            hooks,
            wavecode_core::hooks::HookEventPoint::SessionStart,
            &cwd,
        )
        .await;
    }
    let mut client = InProcessClient::spawn(cfg);
    let mut editor = rustyline::DefaultEditor::new()?;
    // 等待动画仅 TTY 开启；anstream 在非 TTY 下自动剥离样式
    let mut renderer = HumanRenderer::new(anstream::stdout(), is_tty);

    loop {
        // readline 是同步阻塞调用，处于 async 上下文安全：只在无活动 turn
        //（actor 空闲）时调用；未来支持 turn 中并发操作前须迁 spawn_blocking。
        match editor.readline(PROMPT) {
            Ok(line) => {
                let text = line.trim();
                if text.is_empty() {
                    continue;
                }
                if text == "/quit" || text == "/exit" {
                    break;
                }
                // P6：`/memory` 列出持久记忆索引（最简形态：读最新文件，
                // 会话内 memory_write 的新条目同样可见；条目编辑直接改文件）。
                if text == "/memory" {
                    match &memory_root {
                        Some(root) => {
                            let store = wavecode_core::memory::MemoryStore::new(root.clone());
                            match store.read_index() {
                                Ok(index) if index.trim().is_empty() => {
                                    println!(
                                        "（暂无持久记忆；索引文件：{}）",
                                        root.join(wavecode_core::memory::INDEX_FILE).display()
                                    );
                                }
                                Ok(index) => println!("{}", index.trim_end()),
                                Err(e) => eprintln!("读取记忆索引失败：{e}"),
                            }
                        }
                        None => eprintln!("记忆能力不可用（启动时无法解析用户主目录）"),
                    }
                    continue;
                }
                // P9：`/mcp` 列出已配置 server 与状态（首版状态恒为
                // "未连接（transport 未实现）"——诚实展示，不伪造在线状态；
                // 连接与工具清单随真实 transport 落地）。
                if text == "/mcp" {
                    if mcp_lines.is_empty() {
                        println!(
                            "（未配置 MCP server；在 config.toml 添加 [mcp_servers.<name>] 段）"
                        );
                    } else {
                        for line in &mcp_lines {
                            println!("{line}");
                        }
                    }
                    continue;
                }
                // P3：`/compact` 立即压缩（不经 turn）：提交 Op::Compact 后
                // 消费事件到 CompactCompleted / Error 为止，渲染由
                // HumanRenderer 的压缩事件行承担。
                if text == "/compact" {
                    client.submit(new_submission(Op::Compact)).await?;
                    loop {
                        let Some(ev) = client.next_event().await else {
                            println!("\n会话已终止（agent 引擎意外退出）");
                            return Ok(ExitCode::FAILURE);
                        };
                        renderer.handle(&ev)?;
                        if matches!(
                            ev.msg,
                            EventMsg::CompactCompleted { .. } | EventMsg::Error { .. }
                        ) {
                            break;
                        }
                    }
                    continue;
                }
                // P7：`/name [args]` slash 直调 skill（SPEC §8.2）：已知
                // 内置命令（上方精确匹配）之外的 `/` 前缀输入按 skill 名
                // 查找；未命中（或不可直调）按未知命令提示，不进 turn。
                if let Some(rest) = text.strip_prefix('/') {
                    let (name, args) = match rest.split_once(char::is_whitespace) {
                        Some((n, a)) => (n, a.trim()),
                        None => (rest, ""),
                    };
                    let invocable = skill_set
                        .as_ref()
                        .and_then(|set| set.get(name))
                        .is_some_and(|skill| skill.meta.user_invocable);
                    if !invocable {
                        eprintln!(
                            "未知命令：/{name}（内置：/compact /memory /mcp /quit /exit；\
                             其余 / 前缀为 skill 直调，需存在且 user-invocable）"
                        );
                        continue;
                    }
                    let _ = editor.add_history_entry(text);
                    client
                        .submit(new_submission(Op::SlashCommand {
                            name: name.to_owned(),
                            args: args.to_owned(),
                        }))
                        .await?;
                    // inline 是一轮完整 turn；fork 仅见起止事件（终态通知
                    // 按机制在下一 turn 循环头回注）。审批同 UserInput 内联问答。
                    let mut approval = ApprovalHandling::Prompt(&mut editor);
                    let outcome =
                        consume_turn(&mut client, &mut renderer, false, &mut approval).await?;
                    if !matches!(outcome, ConsumeOutcome::TurnCompleted(_)) {
                        println!("\n会话已终止（agent 引擎意外退出）");
                        break;
                    }
                    continue;
                }
                let _ = editor.add_history_entry(text);
                client
                    .submit(new_submission(Op::UserInput {
                        text: text.to_string(),
                    }))
                    .await?;
                // turn 结果（中断 / 错误）不阻断 REPL，渲染即反馈。
                // 审批请求在 consume_turn 内经同一 editor 内联问答（y/n）。
                let mut approval = ApprovalHandling::Prompt(&mut editor);
                let outcome =
                    consume_turn(&mut client, &mut renderer, false, &mut approval).await?;
                if !matches!(outcome, ConsumeOutcome::TurnCompleted(_)) {
                    // actor 意外死亡（事件流提前结束）：先补换行（可能有
                    // 未换行的半个 delta 残留），提示后退出 REPL。
                    println!("\n会话已终止（agent 引擎意外退出）");
                    break;
                }
            }
            Err(rustyline::error::ReadlineError::Interrupted) => continue,
            Err(rustyline::error::ReadlineError::Eof) => break,
            Err(e) => return Err(e.into()),
        }
    }

    // 优雅关闭：submit 后即可退出，不阻塞等待。
    let _ = client.submit(new_submission(Op::Shutdown)).await;
    // P7：SessionEnd hook（清理；记忆自动提取由 actor Shutdown 路径触发）。
    if let Some(hooks) = &hooks {
        run_lifecycle_hooks(
            hooks,
            wavecode_core::hooks::HookEventPoint::SessionEnd,
            &cwd,
        )
        .await;
    }
    Ok(ExitCode::SUCCESS)
}

/// `consume_turn` 的收尾形态。
enum ConsumeOutcome {
    /// 收到 TurnCompleted，附 stop_reason。
    TurnCompleted(StopReason),
    /// 事件流提前结束（actor 退出，未收到 TurnCompleted）。
    StreamEnded,
    /// JSONL 下游断管（如 `| head` 提前关闭管道）：干净结束，非错误。
    BrokenPipe,
}

/// `consume_turn` 的审批处置方式（P2）。
enum ApprovalHandling<'a> {
    /// REPL：收到 ApprovalRequested 时用同一 rustyline editor 内联问答。
    Prompt(&'a mut rustyline::DefaultEditor),
    /// exec（一次性非交互命令）：自动拒绝并回灌固定原因——显式、诚实，
    /// 不做静默放行。
    AutoDeny,
}

/// exec 自动拒绝的回灌原因（模型可据此改用只读方式或说明受阻）。
const NON_INTERACTIVE_DENY_REASON: &str = "non-interactive: approval required";

/// REPL 内联审批问答：y/yes 放行；其余（含 Ctrl-C / Ctrl-D）拒绝，
/// 拒绝后可选填原因（回灌模型，留空由 core 补默认文案）。
///
/// readline 是同步阻塞调用，此处安全：actor 正 park 等待审批回填，
/// 无活动轮询（与主 REPL 循环的 readline 约束同理）。
fn prompt_approval(editor: &mut rustyline::DefaultEditor) -> ApprovalDecision {
    match editor.readline("允许执行？[y/N] ") {
        Ok(line) if matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") => {
            ApprovalDecision::AllowOnce
        }
        Ok(_) => match editor.readline("拒绝原因（回传模型，可留空）：") {
            Ok(reason) => ApprovalDecision::Deny { reason },
            Err(_) => ApprovalDecision::Deny {
                reason: String::new(),
            },
        },
        // Ctrl-C / Ctrl-D：按拒绝处理（不留 park 悬挂）。
        Err(rustyline::error::ReadlineError::Interrupted)
        | Err(rustyline::error::ReadlineError::Eof) => ApprovalDecision::Deny {
            reason: String::new(),
        },
        Err(e) => {
            eprintln!("审批输入错误（按拒绝处理）：{e}");
            ApprovalDecision::Deny {
                reason: String::new(),
            }
        }
    }
}

/// 消费事件流直到 TurnCompleted 并逐事件渲染；事件空闲期由 80ms tick
/// 驱动等待动画（tick_frame 内部按 animate/in_turn 自律，--json 不插入 tick）。
/// ApprovalRequested 按 `approval` 处置并回填 ExecApproval（P2）。
async fn consume_turn<W: std::io::Write>(
    client: &mut InProcessClient,
    renderer: &mut HumanRenderer<W>,
    jsonl: bool,
    approval: &mut ApprovalHandling<'_>,
) -> anyhow::Result<ConsumeOutcome> {
    // Skip 策略：事件密集时丢弃积压 tick，不补帧。
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(80));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            ev = client.next_event() => {
                let Some(ev) = ev else {
                    return Ok(ConsumeOutcome::StreamEnded);
                };
                if jsonl {
                    // JSONL 契约：每行一个完整 Event；flush 保证管道下游流式可见。
                    // 与 HumanRenderer 一致走 io::Result 传播（println! 会在 EPIPE
                    // 时 panic，不可用）；BrokenPipe 特判为干净结束。
                    let mut out = std::io::stdout().lock();
                    let written = writeln!(out, "{}", render::render_jsonl(&ev)).and_then(|()| out.flush());
                    match written {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                            return Ok(ConsumeOutcome::BrokenPipe);
                        }
                        Err(e) => return Err(e.into()),
                    }
                }
                renderer.handle(&ev)?;
                // P2 审批回填：渲染提示行后按处置方式产出决策并提交。
                if let EventMsg::ApprovalRequested { call_id, .. } = &ev.msg {
                    let decision = match approval {
                        ApprovalHandling::Prompt(editor) => prompt_approval(editor),
                        ApprovalHandling::AutoDeny => ApprovalDecision::Deny {
                            reason: NON_INTERACTIVE_DENY_REASON.to_owned(),
                        },
                    };
                    client
                        .submit(new_submission(Op::ExecApproval {
                            call_id: call_id.clone(),
                            decision,
                        }))
                        .await?;
                }
                if let EventMsg::TurnCompleted { stop_reason } = ev.msg {
                    return Ok(ConsumeOutcome::TurnCompleted(stop_reason));
                }
            }
            _ = ticker.tick(), if !jsonl && renderer.is_waiting_on_model() => {
                renderer.tick_frame()?;
            }
        }
    }
}

/// 生成一次 Submission（uuid 关联其后续全部事件）。
fn new_submission(op: Op) -> Submission {
    Submission {
        id: uuid::Uuid::new_v4().to_string(),
        op,
    }
}
