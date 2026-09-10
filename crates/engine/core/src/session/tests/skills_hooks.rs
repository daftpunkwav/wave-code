//! P7 skills 与 hooks：skill 工具 inline/fork / slash 直调 / PreToolUse·Stop·UserPromptSubmit 挂点。

use super::*;

// —— P7：skills 与 hooks（SPEC §8 / §9 场景验收）——

use wavecode_hooks::{HookDef, HookEngine, HookEventPoint};
use wavecode_skills::{Skill, SkillContext, SkillMeta, SkillSet, SkillSource};

/// P7 hook 命令构造（与 hooks crate 测试同款平台适配）：cmd 与 sh 都认
/// `exit N`；stderr 输出分平台写法。stderr 断言用 ASCII（Windows cmd
/// 按 GBK 输出非 ASCII，UTF-8 有损解码会替换）。
fn p7_exit_cmd(code: u32, stderr: &str) -> String {
    if stderr.is_empty() {
        format!("exit {code}")
    } else if cfg!(windows) {
        format!("echo {stderr} 1>&2 & exit {code}")
    } else {
        format!("echo {stderr} 1>&2; exit {code}")
    }
}

/// 超时测试的"睡眠"命令（cmd 无 sleep，用 ping 占位）。
fn p7_sleep_cmd() -> String {
    if cfg!(windows) {
        "ping -n 10 127.0.0.1 >nul".to_owned()
    } else {
        "sleep 10".to_owned()
    }
}

fn p7_hook_def(command: &str) -> HookDef {
    HookDef {
        matcher: None,
        command: command.to_owned(),
        timeout_ms: wavecode_hooks::DEFAULT_TIMEOUT_MS,
        once: false,
    }
}

fn p7_engine(entries: &[(HookEventPoint, HookDef)]) -> Arc<HookEngine> {
    let mut defs: std::collections::HashMap<HookEventPoint, Vec<HookDef>> =
        std::collections::HashMap::new();
    for (point, def) in entries {
        defs.entry(*point).or_default().push(def.clone());
    }
    Arc::new(HookEngine::new(defs))
}

fn p7_skill(name: &str, context: SkillContext, allowed: &[&str], body: &str) -> Skill {
    Skill {
        name: name.to_owned(),
        // 直接以正斜杠字面量构造（join 在 Windows 用反斜杠，断言文本
        // 保持正斜杠形态——路径分隔符本身不在本测试语义内）。
        dir: std::path::PathBuf::from(format!("C:/skills/{name}")),
        source: SkillSource::Project,
        meta: SkillMeta {
            description: format!("{name} 描述"),
            when_to_use: Some("测试触发条件".to_owned()),
            allowed_tools: allowed.iter().map(|s| s.to_string()).collect(),
            context,
            user_invocable: true,
            argument_hint: None,
            paths: vec![],
        },
        body: body.to_owned(),
    }
}

fn p7_skill_set(skills: Vec<Skill>) -> Option<crate::skills::SkillSessionConfig> {
    let mut set = SkillSet::default();
    for skill in skills {
        set.add(skill);
    }
    Some(crate::skills::SkillSessionConfig { set: Arc::new(set) })
}

/// P7 会话构造：bypass 沙箱 + with_subagents（fork 派生面），可挂
/// hooks / skills；模型用 CompactAwareMock（seen 记录全部采样请求，
/// 供"回灌模型"断言）。
fn p7_session(
    model: Arc<CompactAwareMock>,
    dir: &std::path::Path,
    hooks: Option<Arc<HookEngine>>,
    skills: Option<crate::skills::SkillSessionConfig>,
) -> Session {
    Session::with_subagents({
        let (registry, todos) = builtin_registry();
        SessionConfig::builder("mock", model, registry, dir.to_path_buf())
            .todos(todos)
            .sandbox(bypass_sandbox())
            .skills(skills)
            .hooks(hooks)
            .build()
    })
}

/// 采样请求历史的全文（text + tool_result + tool_use 摘要），
/// 供"X 回灌模型出现在后续请求"断言。
fn p7_history_text(req: &ChatRequest) -> String {
    req.messages
        .iter()
        .flat_map(|m| m.content.iter())
        .map(|b| match b {
            ContentBlock::Text { text } => text.clone(),
            ContentBlock::ToolResult { content, .. } => content.clone(),
            ContentBlock::ToolUse { name, input, .. } => format!("[tool_use {name} {input}]"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// skill 工具调用脚本。
fn p7_skill_tool_script(name: &str, args: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolUseBegin {
            id: "t1".into(),
            name: "skill".into(),
        },
        StreamEvent::ToolUseInputDelta {
            partial_json: format!(r#"{{"name":"{name}","args":"{args}"}}"#),
        },
        StreamEvent::BlockEnd,
        StreamEvent::MessageComplete {
            stop_reason: "tool_use".into(),
            usage: Usage::default(),
        },
    ]
}

/// SPEC §8 验收：inline 展开——skill 工具触发展开正文（$ARGUMENTS
/// 替换）并回灌模型（出现在后续采样请求历史）；清单注入（name +
/// description + when_to_use）出现在系统提示词。
#[tokio::test]
async fn skill_tool_inline_expands_arguments_into_next_request() {
    let dir = tempfile::tempdir().unwrap();
    let skills = p7_skill_set(vec![p7_skill(
        "fixit",
        SkillContext::Inline,
        &[],
        "修复 $ARGUMENTS（参考 ${WAVECODE_SKILL_DIR}/notes.md）",
    )]);
    let model = p3_mock(vec![
        Some(p7_skill_tool_script("fixit", "崩溃问题")),
        Some(text_then_end("已修复。")),
    ]);
    let (tx, _rx) = mpsc::channel::<Event>(64);
    let mut session = p7_session(model.clone(), dir.path(), None, skills);
    session.run_turn("s-1", "修一下", tx).await.unwrap();

    let seen = model.seen.lock().unwrap();
    assert!(seen.len() >= 2, "应至少两次采样: {}", seen.len());
    // 清单注入：name + description + when_to_use（SPEC §8.2 注入形态）。
    assert!(
        seen[0]
            .system
            .contains("- fixit: fixit 描述 (when: 测试触发条件)"),
        "清单应注入系统提示词:\n{}",
        seen[0].system
    );
    // inline 展开回灌：$ARGUMENTS 替换 + skill 目录变量替换。
    let history = p7_history_text(&seen[1]);
    assert!(
        history.contains("修复 崩溃问题"),
        "展开正文应回灌:\n{history}"
    );
    assert!(
        history.contains("C:/skills/fixit/notes.md"),
        "skill 目录变量应展开:\n{history}"
    );
}

/// SPEC §8 验收：fork 派生——skill 工具触发后台子代理（SubagentStarted
/// 事件可见、ToolResult 回执 task id）；allowed-tools 按 registry 过滤
/// 子代理工具面（子代理采样请求的 tools 恰为白名单）。
#[tokio::test]
async fn skill_tool_fork_spawns_subagent_with_filtered_registry() {
    let dir = tempfile::tempdir().unwrap();
    let skills = p7_skill_set(vec![p7_skill(
        "deepreview",
        SkillContext::Fork,
        &["read_file"],
        "评审 $ARGUMENTS",
    )]);
    let model = p3_mock(vec![
        Some(p7_skill_tool_script("deepreview", "src/")),
        Some(text_then_end("评审完成。")),
    ]);
    let (tx, mut rx) = mpsc::channel::<Event>(64);
    let mut session = p7_session(model.clone(), dir.path(), None, skills);
    session.run_turn("s-1", "评审一下", tx).await.unwrap();

    // 后台子代理与父会话并发：等 SubagentCompleted 再断言（消除
    // "子代理尚未采样"的竞态），超时兜底防挂死。
    let mut events = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Some(ev) = rx.recv().await {
            let done = matches!(ev.msg, EventMsg::SubagentCompleted { .. });
            events.push(ev.msg);
            if done {
                break;
            }
        }
    })
    .await
    .expect("5s 内应见 SubagentCompleted");
    assert!(
        events.iter().any(|m| matches!(
            m,
            EventMsg::SubagentStarted { description, .. } if description == "skill: deepreview"
        )),
        "应见 skill fork 的 SubagentStarted: {events:?}"
    );
    assert!(
        events.iter().any(|m| matches!(
            m,
            EventMsg::ToolCallEnd { ok: true, output, .. } if output.contains("task-")
        )),
        "skill 工具回执应含 task id: {events:?}"
    );
    // allowed-tools 过滤：子代理采样请求的 tools 恰为 ["read_file"]
    //（父会话请求带全量工具，按此特征定位子代理请求，与调度顺序无关）。
    let seen = model.seen.lock().unwrap();
    let child_req = seen
        .iter()
        .find(|req| {
            let mut names: Vec<&str> = req.tools.iter().map(|t| t.name.as_str()).collect();
            names.sort_unstable();
            names == ["read_file"]
        })
        .expect("子代理请求的工具面应被 allowed-tools 过滤");
    // fork 指令：skill 正文（preamble）拼在子代理输入前部，args 进 prompt。
    let child_input = p7_history_text(child_req);
    assert!(child_input.contains("评审 src/"), "{child_input}");
}

/// SPEC §8 验收：`/name [args]` slash 直调（invoke_skill）——inline
/// 展开正文作为 turn 输入（历史首条 user 消息含展开文本）。
#[tokio::test]
async fn slash_inline_skill_expands_as_turn_input() {
    let dir = tempfile::tempdir().unwrap();
    let skills = p7_skill_set(vec![p7_skill(
        "fixit",
        SkillContext::Inline,
        &[],
        "修复 $ARGUMENTS",
    )]);
    let model = p3_mock(vec![Some(text_then_end("done"))]);
    let (tx, mut rx) = mpsc::channel::<Event>(64);
    let mut session = p7_session(model.clone(), dir.path(), None, skills);
    session
        .invoke_skill("s-1", "fixit", "崩溃问题", tx)
        .await
        .unwrap();

    {
        let seen = model.seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "inline slash 应驱动一轮 turn");
        assert!(
            p7_history_text(&seen[0]).contains("修复 崩溃问题"),
            "展开正文应为 turn 输入"
        );
    }
    let events = collect_events(&mut rx);
    assert!(
        events
            .iter()
            .any(|m| matches!(m, EventMsg::TurnCompleted { .. })),
        "slash 交互应以 TurnCompleted 收尾: {events:?}"
    );
    // 未知 skill：Error + TurnCompleted，不发起采样。
    let (tx, mut rx) = mpsc::channel::<Event>(64);
    session.invoke_skill("s-2", "nope", "", tx).await.unwrap();
    let events = collect_events(&mut rx);
    assert!(
        events.iter().any(|m| matches!(
            m,
            EventMsg::Error { message, .. } if message.contains("unknown skill: nope")
        )),
        "{events:?}"
    );
    assert!(model.seen.lock().unwrap().len() == 1, "未知名不得发起采样");
}

/// SPEC §9 验收：PreToolUse 阻塞——退出码 2，工具不执行，stderr 回灌
/// 模型（出现在后续采样请求历史）。
#[tokio::test]
async fn pre_tool_use_hook_blocks_and_stderr_reaches_model() {
    let dir = tempfile::tempdir().unwrap();
    let engine = p7_engine(&[(
        HookEventPoint::PreToolUse,
        HookDef {
            matcher: Some("write_file".to_owned()),
            ..p7_hook_def(&p7_exit_cmd(2, "no-writes-today"))
        },
    )]);
    let model = p3_mock(vec![
        Some(write_file_script("blocked.txt", "x")),
        Some(text_then_end("被拦了。")),
    ]);
    let (tx, mut rx) = mpsc::channel::<Event>(64);
    let mut session = p7_session(model.clone(), dir.path(), Some(engine), None);
    session.run_turn("s-1", "写文件", tx).await.unwrap();

    assert!(
        !dir.path().join("blocked.txt").exists(),
        "阻塞的工具不得执行"
    );
    let events = collect_events(&mut rx);
    assert!(
        events
            .iter()
            .any(|m| matches!(m, EventMsg::ToolCallEnd { ok: false, .. })),
        "阻塞应产生失败 ToolCallEnd: {events:?}"
    );
    let seen = model.seen.lock().unwrap();
    let history = p7_history_text(&seen[1]);
    assert!(
        history.contains("no-writes-today"),
        "stderr 应回灌模型:\n{history}"
    );
}

/// SPEC §9 验收：退出码 1——警告放行（Warning 事件可见，工具照常执行）。
#[tokio::test]
async fn pre_tool_use_hook_exit1_warns_and_allows() {
    let dir = tempfile::tempdir().unwrap();
    let engine = p7_engine(&[(
        HookEventPoint::PreToolUse,
        p7_hook_def(&p7_exit_cmd(1, "hook-oops")),
    )]);
    let model = p3_mock(vec![
        Some(write_file_script("ok.txt", "x")),
        Some(text_then_end("done")),
    ]);
    let (tx, mut rx) = mpsc::channel::<Event>(64);
    let mut session = p7_session(model.clone(), dir.path(), Some(engine), None);
    session.run_turn("s-1", "写文件", tx).await.unwrap();

    assert!(dir.path().join("ok.txt").exists(), "警告放行的工具应执行");
    let events = collect_events(&mut rx);
    assert!(
        events.iter().any(|m| matches!(
            m,
            EventMsg::Warning { message } if message.contains("退出码 1") && message.contains("hook-oops")
        )),
        "退出码 1 应转 Warning 事件: {events:?}"
    );
}

/// SPEC §9 验收：超时强制 kill 记 warning（工具照常执行；hook 不拖死 turn）。
#[tokio::test]
async fn pre_tool_use_hook_timeout_killed_warns() {
    let dir = tempfile::tempdir().unwrap();
    let engine = p7_engine(&[(
        HookEventPoint::PreToolUse,
        HookDef {
            timeout_ms: 200,
            ..p7_hook_def(&p7_sleep_cmd())
        },
    )]);
    let model = p3_mock(vec![
        Some(write_file_script("ok.txt", "x")),
        Some(text_then_end("done")),
    ]);
    let (tx, mut rx) = mpsc::channel::<Event>(64);
    let mut session = p7_session(model.clone(), dir.path(), Some(engine), None);
    session.run_turn("s-1", "写文件", tx).await.unwrap();

    assert!(
        dir.path().join("ok.txt").exists(),
        "超时警告放行的工具应执行"
    );
    let events = collect_events(&mut rx);
    assert!(
        events.iter().any(|m| matches!(
            m,
            EventMsg::Warning { message } if message.contains("超时")
        )),
        "超时 kill 应转 Warning 事件: {events:?}"
    );
}

/// SPEC §9 验收：matcher 匹配——matcher 未命中的工具不触发 hook
///（无警告、正常执行）；命中的工具才触发。
#[tokio::test]
async fn pre_tool_use_hook_matcher_filters_tools() {
    let dir = tempfile::tempdir().unwrap();
    let engine = p7_engine(&[(
        HookEventPoint::PreToolUse,
        HookDef {
            matcher: Some("shell".to_owned()),
            ..p7_hook_def(&p7_exit_cmd(2, "should-not-fire"))
        },
    )]);
    let model = p3_mock(vec![
        Some(write_file_script("ok.txt", "x")),
        Some(text_then_end("done")),
    ]);
    let (tx, mut rx) = mpsc::channel::<Event>(64);
    let mut session = p7_session(model.clone(), dir.path(), Some(engine), None);
    session.run_turn("s-1", "写文件", tx).await.unwrap();

    assert!(
        dir.path().join("ok.txt").exists(),
        "matcher 未命中时工具应正常执行"
    );
    let events = collect_events(&mut rx);
    assert!(
        !events.iter().any(|m| matches!(m, EventMsg::Warning { .. })),
        "matcher 未命中不得产生 hook 警告: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|m| matches!(m, EventMsg::ToolCallEnd { ok: true, .. })),
        "{events:?}"
    );
}

/// SPEC §9 / §5.2 验收：Stop hook 阻塞——stderr 作为 user 消息回灌模型
/// 继续 turn；once 语义下第二次收尾放行（次序：先 todo steering 后
/// Stop hook，本测试清单为空直接到 Stop hook）。
#[tokio::test]
async fn stop_hook_blocks_once_then_completes() {
    let dir = tempfile::tempdir().unwrap();
    let engine = p7_engine(&[(
        HookEventPoint::Stop,
        HookDef {
            once: true,
            ..p7_hook_def(&p7_exit_cmd(2, "goal-not-met"))
        },
    )]);
    let model = p3_mock(vec![
        Some(text_then_end("先收工。")),
        Some(text_then_end("补完了。")),
    ]);
    let (tx, mut rx) = mpsc::channel::<Event>(64);
    let mut session = p7_session(model.clone(), dir.path(), Some(engine), None);
    let reason = session.run_turn("s-1", "干活", tx).await.unwrap();
    assert_eq!(reason, StopReason::Completed);

    let seen = model.seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "Stop 阻塞应驱动额外一轮采样");
    let history = p7_history_text(&seen[1]);
    assert!(
        history.contains("goal-not-met"),
        "Stop hook stderr 应回灌模型:\n{history}"
    );
    drop(seen);
    let events = collect_events(&mut rx);
    assert!(
        events.iter().any(|m| matches!(
            m,
            EventMsg::Warning { message } if message.contains("Stop hook blocked")
        )),
        "{events:?}"
    );
}

/// UserPromptSubmit hook 阻塞：输入不进历史、不发起采样，stderr 以
/// Error 事件展示 + TurnCompleted 收尾（前端不悬挂）。
#[tokio::test]
async fn user_prompt_submit_hook_blocks_turn() {
    let dir = tempfile::tempdir().unwrap();
    let engine = p7_engine(&[(
        HookEventPoint::UserPromptSubmit,
        p7_hook_def(&p7_exit_cmd(2, "blocked-word")),
    )]);
    let model = p3_mock(vec![Some(text_then_end("不应到达"))]);
    let (tx, mut rx) = mpsc::channel::<Event>(64);
    let mut session = p7_session(model.clone(), dir.path(), Some(engine), None);
    session.run_turn("s-1", "敏感输入", tx).await.unwrap();

    assert!(
        model.seen.lock().unwrap().is_empty(),
        "阻塞的输入不得发起采样"
    );
    let events = collect_events(&mut rx);
    assert!(
        events.iter().any(|m| matches!(
            m,
            EventMsg::Error { message, .. } if message.contains("blocked-word")
        )),
        "{events:?}"
    );
    assert!(
        events
            .iter()
            .any(|m| matches!(m, EventMsg::TurnCompleted { .. })),
        "{events:?}"
    );
}
