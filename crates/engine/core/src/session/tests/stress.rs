//! P10 长程硬化：≥50 轮压缩循环压力测试 + 泄漏粗检（CountingAlloc）。

use super::*;

// —— P10 长程硬化：压缩循环压力测试（≥50 轮）+ 泄漏粗检 ——

/// P10 泄漏粗检：计数分配器（统计活跃分配字节 = 累计 alloc − dealloc）。
/// 精度边界（诚实声明）：这是"活跃分配字节"快照而非 RSS——RSS 受分配器
/// 缓存与碎片影响，且无可移植读法（Windows 无 /proc）；tokio 任务数无
/// 稳定 API；句柄泄漏无便携探测。故本断言只锁定"活跃内存不随轮次线性
/// 增长"这一代理指标，RSS / 句柄 / 任务数级泄漏由人工长跑验收覆盖
///（scripts/acceptance/ecommerce.md）。
struct CountingAlloc;

static LIVE_BYTES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

unsafe impl std::alloc::GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        LIVE_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        // SAFETY: 透传系统分配器；layout 有效性由调用方（运行时）保证。
        unsafe { std::alloc::GlobalAlloc::alloc(&std::alloc::System, layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: 透传系统分配器；ptr/layout 与 alloc 配对由运行时保证。
        unsafe { std::alloc::GlobalAlloc::dealloc(&std::alloc::System, ptr, layout) }
    }
}

#[global_allocator]
static P10_LEAK_CHECK_ALLOC: CountingAlloc = CountingAlloc;

/// P10 压力 mock：采样恒回 99_950 input_tokens（过自动压缩线，每个
/// turn 的 PreTurn 触发一次压缩）；摘要"引用前次摘要"——从历史首条的
/// 上一轮摘要解析迭代号并 +1（模拟真实摘要的信息链传递），解析失败
/// 产出 CHAIN-BROKEN 标记（断链在最终断言可见）。
struct StressMock {
    summary_calls: Mutex<usize>,
}

/// 从上一轮摘要正文解析"第 N 轮迭代完成"的迭代号。
fn p10_parse_round(summary: &str) -> Option<usize> {
    let start = summary.find("第 ")? + "第 ".len();
    let end = summary[start..].find(" 轮迭代完成")? + start;
    summary[start..end].parse().ok()
}

#[async_trait::async_trait]
impl ChatModel for StressMock {
    async fn stream(
        &self,
        req: ChatRequest,
    ) -> wavecode_llm::Result<
        std::pin::Pin<Box<dyn futures::Stream<Item = wavecode_llm::Result<StreamEvent>> + Send>>,
    > {
        if req.tools.is_empty() {
            // 摘要请求（ModelSummary 不带工具，与 p3 mock 同判定）。
            let prev_summary = req.messages.first().and_then(|m| {
                m.content.iter().find_map(|b| match b {
                    ContentBlock::Text { text }
                        if text.starts_with(wavecode_context::SUMMARY_MESSAGE_PREFIX) =>
                    {
                        Some(text.clone())
                    }
                    _ => None,
                })
            });
            let round = match &prev_summary {
                None => Some(1usize),
                Some(text) => p10_parse_round(text).map(|r| r + 1),
            };
            *self.summary_calls.lock().unwrap() += 1;
            let body = match round {
                Some(round) => format!(
                    "## 目标\n搭建电商平台（GOAL-ANCHOR）。\n## 进展\n第 {round} 轮迭代完成（引用前次摘要：第 {} 轮）。\n## 关键决策\n首版 SQLite，零运维（DECISION-ANCHOR）。\n## 文件清单\ncrates/shop/src/cart.rs 已创建（FILE-ANCHOR）。\n## 待办\n第 {} 轮迭代（TODO-ANCHOR）。",
                    round.saturating_sub(1),
                    round + 1
                ),
                None => "CHAIN-BROKEN".to_owned(),
            };
            Ok(Box::pin(stream::iter(vec![
                Ok(StreamEvent::TextDelta { text: body }),
                Ok(StreamEvent::MessageComplete {
                    stop_reason: "end_turn".into(),
                    usage: Usage {
                        input_tokens: 100,
                        output_tokens: 20,
                    },
                }),
            ])))
        } else {
            // 采样：纯文本终态 + 过自动线水位（下一 turn PreTurn 压缩）。
            Ok(Box::pin(stream::iter(vec![
                Ok(StreamEvent::TextDelta {
                    text: "本轮完成。".into(),
                }),
                Ok(StreamEvent::MessageComplete {
                    stop_reason: "end_turn".into(),
                    usage: Usage {
                        input_tokens: 99_950,
                        output_tokens: 5,
                    },
                }),
            ])))
        }
    }
}

/// P10 验收锚点（DEV-PLAN §0 总目标 2 的代理指标）：压缩循环压力
/// 测试——mock 驱动 50 轮 turn，每轮 PreTurn 自动压缩一次；每轮断言
/// 配对零违规；50 轮后五要素锚点链（目标/决策/文件清单/待办）在
/// "摘要引用前次摘要"的传递下完整可追溯，历史条数有界。
/// 附泄漏粗检：活跃分配字节增量有界（精度边界见 CountingAlloc 注释）。
#[tokio::test]
async fn compaction_loop_stress_50_rounds_no_pairing_violations() {
    const ROUNDS: usize = 50;
    let dir = tempfile::tempdir().unwrap();
    let model = Arc::new(StressMock {
        summary_calls: Mutex::new(0),
    });
    let mut session = Session::new({
        let (registry, todos) = builtin_registry();
        SessionConfig::builder("mock", model.clone(), registry, dir.path().to_path_buf())
            .todos(todos)
            .context_window(100_000)
            .sandbox(bypass_sandbox())
            .context(ContextConfig {
                thresholds: wavecode_context::Thresholds {
                    warning_margin: 200,
                    auto_compact_margin: 100,
                    blocking_margin: 10,
                },
                keep_recent: 2,
                summary_max_tokens: 500,
                estimate_chars_per_token: 4,
            })
            // 压力测试同时压 rollout 记录面（每轮压缩记录 + 消息记录）。
            .rollout(p10_rollout(dir.path(), "t-stress"))
            .build()
    });
    // 种子水位：每个 turn 的首次 PreTurn 检查即触发自动压缩。
    session.usage_carry = Some(99_950);

    // 泄漏粗检基线：先跑 2 轮热身（惰性初始化 / 一次性分配不计入）。
    for i in 0..2 {
        let (tx, _rx) = mpsc::channel::<Event>(64);
        session
            .run_turn(&format!("s-{i}"), "继续迭代", tx)
            .await
            .unwrap();
    }
    let live_before = LIVE_BYTES.load(Ordering::Relaxed);

    for i in 2..ROUNDS {
        let (tx, _rx) = mpsc::channel::<Event>(64);
        session
            .run_turn(&format!("s-{i}"), "继续迭代", tx)
            .await
            .unwrap();
        assert_eq!(
            wavecode_context::find_pairing_violations(&session.messages),
            Vec::<String>::new(),
            "第 {i} 轮压缩后配对违规: {:?}",
            session.messages
        );
    }
    let live_after = LIVE_BYTES.load(Ordering::Relaxed);

    // 每轮恰一次压缩。
    assert_eq!(*model.summary_calls.lock().unwrap(), ROUNDS);
    // 历史有界：压缩稳态下条数不随轮次增长（摘要 + 保留尾 + 本轮收发）。
    assert!(
        session.messages.len() <= 6,
        "历史条数应有界: {}",
        session.messages.len()
    );
    // 无请求快照滞留：turn 结束后历史 Arc 唯一持有（泄漏的常见形态）。
    assert_eq!(Arc::strong_count(&session.messages), 1);

    // 五要素信息链：50 轮压缩后锚点仍可追溯，迭代号连续未断链。
    let ContentBlock::Text { text } = &session.messages[0].content[0] else {
        panic!("首条应为摘要文本消息: {:?}", session.messages)
    };
    assert!(
        text.starts_with(wavecode_context::SUMMARY_MESSAGE_PREFIX),
        "{text}"
    );
    assert!(!text.contains("CHAIN-BROKEN"), "摘要信息链断裂: {text}");
    for anchor in [
        "GOAL-ANCHOR",
        "DECISION-ANCHOR",
        "FILE-ANCHOR",
        "TODO-ANCHOR",
    ] {
        assert!(
            text.contains(anchor),
            "50 轮压缩后缺要素锚点「{anchor}」: {text}"
        );
    }
    assert!(
        text.contains("第 50 轮迭代完成"),
        "迭代号应链式传递到第 50 轮: {text}"
    );

    // rollout 记录面同步受压：50 条压缩记录 + 每轮 2 条消息（首轮另
    // 有种子输入外的 user 消息……精确计数 = 50 压缩 + 100 消息）。
    let load = crate::rollout::load_rollout(&dir.path().join("threads/t-stress.jsonl")).unwrap();
    let compactions = load
        .records
        .iter()
        .filter(|r| matches!(r, crate::rollout::RolloutRecord::Compaction { .. }))
        .count();
    assert_eq!(compactions, ROUNDS);
    assert_eq!(load.records.len(), ROUNDS * 3);
    let seqs = p10_seqs(&load);
    assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1), "seq 全程连续");

    // 泄漏粗检（best-effort，精度边界见 CountingAlloc 注释）：活跃
    // 分配增量有界——稳态下每轮的分配应在 turn 结束时释放；阈值取
    // 宽裕常数以吸收并行测试的瞬时分配噪声。
    let delta = live_after.saturating_sub(live_before);
    assert!(
        delta < 16 * 1024 * 1024,
        "活跃分配增量 {delta} 字节超界（疑似随轮次增长的泄漏）"
    );
}
