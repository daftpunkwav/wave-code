//! 行式 REPL（阶段 4 拆分自 main.rs）：交互循环、审批处理、turn 消费。

use super::*;

pub(crate) async fn run_repl(
    cfg: SessionConfig,
    mcp_lines: Vec<String>,
) -> anyhow::Result<ExitCode> {
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
            write!(out, "\r{}", banner::frame(7, phase))?;
            out.flush()?;
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        }
    }
    anstream::print!(
        "{}",
        banner::banner(&cfg.model_name, &cfg.cwd, version, phase)
    );

    // P7：`/name [args]` slash 直调的查找面（cfg 移入前取出）。
    let skill_set = cfg.skills.as_ref().map(|s| s.set.clone());
    // SessionStart/SessionEnd hook 已收口 actor：警告经 Warning 事件进
    // 事件流，由渲染器呈现。
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
                // P6：`/memory` 列出持久记忆索引——走协议面（Op::MemoryList
                // → EventMsg::MemoryIndex），与 TUI 同一面；会话内
                // memory_write 的新条目同样可见（core 侧现读存储）。
                if text == "/memory" {
                    client.submit(new_submission(Op::MemoryList)).await?;
                    loop {
                        let Some(ev) = client.next_event().await else {
                            println!("
会话已终止（agent 引擎意外退出）");
                            return Ok(ExitCode::FAILURE);
                        };
                        renderer.handle(&ev)?;
                        if let EventMsg::MemoryIndex { path, content } = ev.msg {
                            match path {
                                Some(p) if content.trim().is_empty() => {
                                    println!("（暂无持久记忆；索引文件：{p}）");
                                }
                                Some(_) => println!("{}", content.trim_end()),
                                None => eprintln!("记忆能力不可用（会话未启用记忆装配）"),
                            }
                            break;
                        }
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

    // 优雅关闭：submit 后即可退出（SessionEnd hook 与记忆提取由 actor
    // Shutdown 路径处理，尽力而为）。
    let _ = client.submit(new_submission(Op::Shutdown)).await;
    Ok(ExitCode::SUCCESS)
}

/// `consume_turn` 的收尾形态。
pub(crate) enum ConsumeOutcome {
    /// 收到 TurnCompleted，附 stop_reason。
    TurnCompleted(StopReason),
    /// 事件流提前结束（actor 退出，未收到 TurnCompleted）。
    StreamEnded,
    /// JSONL 下游断管（如 `| head` 提前关闭管道）：干净结束，非错误。
    BrokenPipe,
}

/// `consume_turn` 的审批处置方式（P2）。
pub(crate) enum ApprovalHandling<'a> {
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
pub(crate) fn prompt_approval(editor: &mut rustyline::DefaultEditor) -> ApprovalDecision {
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
pub(crate) async fn consume_turn<W: std::io::Write>(
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
pub(crate) fn new_submission(op: Op) -> Submission {
    Submission {
        id: uuid::Uuid::new_v4().to_string(),
        op,
    }
}
