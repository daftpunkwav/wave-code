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

mod banner;
mod markdown;
mod render;

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use wavecode_app_server::InProcessClient;
use wavecode_core::SessionConfig;
use wavecode_protocol::{ApprovalDecision, EventMsg, Op, PermissionMode, StopReason, Submission};

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

    // 装配根在 core（SPEC §3 收口）：cli 只解析运行环境（cwd / home）
    // 与呈现警告。cwd 无法确定 = 运行时错误（退出码 1）。
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            eprintln!("错误：无法确定当前工作目录: {e}");
            return Ok(ExitCode::from(1));
        }
    };
    let home = wavecode_config::home_dir();
    // New-stack TUI path branches before the legacy assembly: interactive
    // fullscreen sessions run on the harness engine; exec, REPL, and
    // resume still use the legacy assembly during the transition.
    if cli.command.is_none() && !cli.repl && std::io::stdout().is_terminal() {
        return run_tui_new(cli.config, cli.model, cwd, home).await;
    }
    let boot = match wavecode_core::assemble::load_boot(
        cli.config.as_deref(),
        cli.model.as_deref(),
        &cwd,
        home.as_deref(),
    ) {
        Ok(boot) => boot,
        Err(wavecode_core::assemble::BootError::Config(e)) => {
            // 配置缺失 / 加载失败：中文指引 + 退出码 2。
            print_config_error(&e);
            return Ok(ExitCode::from(2));
        }
    };
    for warning in &boot.warnings {
        eprintln!("警告：{warning}");
    }
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
        Some(Command::Resume { thread_id }) => {
            run_resume(cfg, mcp_lines, thread_id, cli.repl).await
        }
        // P8 TTY 路径已在上游提前进入新栈 TUI；此处 None 仅剩行式 REPL
        //（--repl 或非 TTY 降级路径，渲染语义不变）。
        None => run_repl(cfg, mcp_lines).await,
    }
}

/// P10：`wavecode resume [thread-id]`（SPEC §16）。无 id 时列出最近会话
///（rollout 文件 mtime 倒序 + 首条用户消息摘要；SQLite 索引的首版降级
/// 形态，见 core::rollout 模块注释）；有 id 时经
/// [`wavecode_core::rollout::inspect_thread`] 校验并预览，覆盖 boot 分配的
/// thread id 后进入交互界面（构造即 replay 恢复）。resume 保持 cli 启动期
/// 功能、不做协议化（SPEC §3:111 豁免：恢复发生在 actor 存在之前）；
/// rollout 布局与回放知识收口在 core::rollout，cli 只消费意图级 API。
async fn run_resume(
    mut cfg: SessionConfig,
    mcp_lines: Vec<String>,
    thread_id: Option<String>,
    force_repl: bool,
) -> anyhow::Result<ExitCode> {
    use wavecode_core::rollout::ThreadLookupError;
    let Some(home) = wavecode_config::home_dir() else {
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
    let info = match wavecode_core::rollout::inspect_thread(&root, &id) {
        Ok(info) => info,
        Err(ThreadLookupError::InvalidId(id)) => {
            eprintln!("错误：非法 thread-id {id:?}（仅允许字母数字 / - / _）");
            return Ok(ExitCode::from(2));
        }
        Err(ThreadLookupError::NotFound(path)) => {
            eprintln!(
                "错误：找不到会话 {id}（{}）；`wavecode resume` 可列出最近会话",
                path.display()
            );
            return Ok(ExitCode::from(2));
        }
        // 解构出内层 io::Error 直接入 anyhow：与改动前 `?` 传播单层错误链
        // 同构（经 ThreadLookupError 的 transparent 包装会产生重复的
        // Caused by 段）。
        Err(ThreadLookupError::Io(e)) => return Err(e.into()),
    };
    // 恢复信息行（Session 构造会再 replay 一次；rollout 文件为小文件，
    // 双读可接受）。
    println!(
        "已恢复会话 {id}（{} 条消息 / {} 次压缩）",
        info.message_count, info.compaction_count
    );
    cfg.rollout = Some(wavecode_core::rollout::RolloutConfig {
        root,
        thread_id: id,
    });
    // 与默认命令同纪律：TTY 进新栈 TUI，非 TTY 或 --repl 回退行式 REPL。
    // rollout 回放尚未移植到新栈：恢复会话从新会话启动并明确提示，
    // 历史重放随后补上（旧 replay 语义不变，仅延迟）。
    if force_repl || !std::io::stdout().is_terminal() {
        run_repl(cfg, mcp_lines).await
    } else {
        eprintln!("警告：会话恢复的历史回放尚未移植，新会话启动（恢复信息见上）");
        let cwd = std::env::current_dir()?;
        run_tui_new(None, None, cwd, wavecode_config::home_dir()).await
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
    // SessionStart/SessionEnd hook 已收口 actor（InProcessClient 内）：
    // 警告经 Warning 事件进事件流，--json 下进 JSONL、人类模式下由渲染器呈现。
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

    // 通知 actor 优雅关闭并排干事件流直到 actor 退出：SessionEnd hook 的
    // Warning 与 TurnCompleted 后的事件由此呈现（限 5s，防 hook 挂死拖住退出）。
    let _ = client.submit(new_submission(Op::Shutdown)).await;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while let Some(ev) = tokio::time::timeout_at(deadline, client.next_event())
        .await
        .ok()
        .flatten()
    {
        renderer.handle(&ev)?;
    }

    Ok(match outcome {
        // 断管（下游如 `| head` 提前关闭）：用户已取走所需输出，干净退出。
        ConsumeOutcome::TurnCompleted(StopReason::Completed) | ConsumeOutcome::BrokenPipe => {
            ExitCode::SUCCESS
        }
        _ => ExitCode::FAILURE,
    })
}

/// New-stack ratatui TUI entry: assembles a live harness session and
/// maps it onto the ported [`wavecode_tui::TuiContext`]. Interactive
/// approval parking works here (headless stays false); config failures
/// keep the legacy exit-code contract (2 with guidance).
async fn run_tui_new(
    config: Option<PathBuf>,
    model: Option<String>,
    cwd: PathBuf,
    home: Option<PathBuf>,
) -> anyhow::Result<ExitCode> {
    let handle =
        match operations_bootstrap::assemble_session(operations_bootstrap::AssembleOptions {
            config_path: config,
            model_override: model,
            cwd: cwd.clone(),
            home,
            identity: operations_bootstrap::DEFAULT_IDENTITY.to_string(),
            headless: false,
        }) {
            Ok(handle) => handle,
            Err(operations_bootstrap::SessionError::Config(e)) => {
                print_config_error(&e);
                return Ok(ExitCode::from(2));
            }
        };
    for warning in &handle.warnings {
        eprintln!("警告：{warning}");
    }
    let ctx = wavecode_tui::TuiContext {
        model_name: handle.model_name,
        cwd,
        permission_mode: match handle.permission_mode.as_str() {
            "plan" => wavecode_tui::PermissionMode::Plan,
            "acceptEdits" => wavecode_tui::PermissionMode::AcceptEdits,
            "bypassPermissions" => wavecode_tui::PermissionMode::BypassPermissions,
            _ => wavecode_tui::PermissionMode::Default,
        },
        skill_names: handle.skill_names,
        mcp_server_lines: handle.mcp_servers,
        memory_index: handle.memory_index,
    };
    wavecode_tui::run(handle.client, ctx).await?;
    Ok(ExitCode::SUCCESS)
}

/// 基础交互 REPL：`/quit`、`/exit` 退出；空行跳过；其余输入作为 UserInput
/// 提交，流式渲染至 TurnCompleted 后回到提示符。Ctrl-D 退出，Ctrl-C 放弃
/// 当前输入行。
mod repl;

use crate::repl::{ApprovalHandling, ConsumeOutcome, consume_turn, new_submission, run_repl};
