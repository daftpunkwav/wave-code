//! wavecode-context — 上下文管理管线（单一实现，策略可插拔）。
//!
//! 单一管线三个阶段（SPEC §6）：
//! 1. **核算**：优先使用 provider 回传的 usage（`input_tokens` 已是覆盖完整
//!    历史的权威值）；无 usage（首个 turn / 压缩后未再采样）时回退
//!    [`estimate_tokens`] 字符估算。
//! 2. **三级阈值**（[`Thresholds`]，按窗口比例参数化，默认值对齐 SPEC §6）：
//!    警告线 window-20k / 自动压缩线 window-13k / 阻塞线 window-3k。
//! 3. **压缩**：[`CompactionStrategy`] trait 抽象（可替换），首版实现
//!    [`ModelSummary`]（一次模型调用生成五要素结构化摘要）；新历史 =
//!    摘要消息 + 最近 N 条原文，经 [`normalize_history`] 保证配对完整。
//!
//! 本 crate 只依赖 `wavecode-llm`（SPEC §3 矩阵）；触发时序由 core 编排。

use std::sync::Arc;

use futures::StreamExt;
use wavecode_llm::{ChatModel, ChatRequest, ContentBlock, Message, Role, StreamEvent};

// ---------------------------------------------------------------------------
// token 核算
// ---------------------------------------------------------------------------

/// 字符估算比率的默认值（字符/token）。
pub const DEFAULT_CHARS_PER_TOKEN: usize = 4;

/// 系统提示词与工具清单的固定开销定额（SPEC §6 "预计系统开销"）。
/// 粗略定额：系统提示词模板 ~百级 token + 内置工具 schema ~1–2k token；
/// 只在估算路径（无 usage）参与，usage 路径的 input_tokens 已含全部开销。
pub const SYSTEM_OVERHEAD_TOKENS: u64 = 2_000;

/// 历史消息的 token 估算（无 provider usage 时的回退路径）。
///
/// 误差边界（须知晓，勿当权威值）：英文/代码文本 ~4 字符/token（±20%）；
/// 中文 ~1.5–2 字符/token，本估算对中文历史可低估约一半。因此三级阈值的
/// 触发以 usage 为准，估算只用于"还从未拿到 usage"的窗口（首个 turn、
/// 压缩后未再采样），误差由阈值的 margin 量级（≥3k）兜底。
pub fn estimate_tokens(messages: &[Message], chars_per_token: usize) -> u64 {
    let ratio = chars_per_token.max(1) as u64;
    let mut chars = 0u64;
    for m in messages {
        for b in &m.content {
            chars += match b {
                ContentBlock::Text { text } => text.chars().count() as u64,
                ContentBlock::ToolUse { name, input, .. } => {
                    name.chars().count() as u64 + input.to_string().chars().count() as u64
                }
                ContentBlock::ToolResult { content, .. } => content.chars().count() as u64,
            };
        }
    }
    // 每条消息的结构开销（role / 块框架）按 ~4 token 定额。
    chars / ratio + 4 * messages.len() as u64
}

// ---------------------------------------------------------------------------
// 三级阈值
// ---------------------------------------------------------------------------

/// 预算水位（[`Thresholds::check`] 的判定结果，逐级加深）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BudgetLevel {
    /// 充裕。
    Ok,
    /// 警告线：`used ≥ window - warning_margin`——提示"接近上限"。
    Warning,
    /// 自动压缩线：`used ≥ window - auto_compact_margin`——触发压缩。
    AutoCompact,
    /// 阻塞线：`used ≥ window - blocking_margin`——强制先压缩再采样。
    Blocking,
}

/// 三级阈值（SPEC §6，默认值对齐 Claude Code 实测值）。
///
/// 以"距窗口上沿的 margin"参数化而非比例浮点数：20k/13k/3k 是 token 量纲
/// 的实测经验值，不同窗口大小下直接平移即可；如需按窗口比例配置，由
/// 调用方（配置层）换算成本结构体的 margin。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thresholds {
    /// 警告线 margin（默认 20_000）。
    pub warning_margin: u64,
    /// 自动压缩线 margin（默认 13_000）。
    pub auto_compact_margin: u64,
    /// 阻塞线 margin（默认 3_000）。
    pub blocking_margin: u64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            warning_margin: 20_000,
            auto_compact_margin: 13_000,
            blocking_margin: 3_000,
        }
    }
}

impl Thresholds {
    /// 判定 `used / window` 所处的水位（取最深一级）。
    ///
    /// `window` 小于 margin 时按 saturating 处理（水位线压到 0，即任何
    /// 占用都触发最深级别）——配置错误宁可过度压缩，不可静默越窗。
    pub fn check(&self, used: u64, window: u64) -> BudgetLevel {
        if used >= window.saturating_sub(self.blocking_margin) {
            BudgetLevel::Blocking
        } else if used >= window.saturating_sub(self.auto_compact_margin) {
            BudgetLevel::AutoCompact
        } else if used >= window.saturating_sub(self.warning_margin) {
            BudgetLevel::Warning
        } else {
            BudgetLevel::Ok
        }
    }
}

// ---------------------------------------------------------------------------
// 压缩
// ---------------------------------------------------------------------------

/// 压缩后保留的最近原文消息条数默认值（SPEC §6 "默认 10"）。
pub const DEFAULT_KEEP_RECENT: usize = 10;

/// 摘要调用的默认输出预算（max_tokens）。
pub const DEFAULT_SUMMARY_MAX_TOKENS: u32 = 4096;

/// 上下文管线配置（core 的 SessionConfig 内嵌一份，构造后冻结）。
#[derive(Debug, Clone)]
pub struct ContextConfig {
    /// 三级阈值。
    pub thresholds: Thresholds,
    /// 压缩后保留的最近原文消息条数。
    pub keep_recent: usize,
    /// 摘要调用的输出预算（max_tokens）。
    pub summary_max_tokens: u32,
    /// 无 usage 回退估算的字符/token 比率（误差边界见 [`estimate_tokens`]）。
    pub estimate_chars_per_token: usize,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            thresholds: Thresholds::default(),
            keep_recent: DEFAULT_KEEP_RECENT,
            summary_max_tokens: DEFAULT_SUMMARY_MAX_TOKENS,
            estimate_chars_per_token: DEFAULT_CHARS_PER_TOKEN,
        }
    }
}

/// crate 统一错误类型。
#[derive(Debug, thiserror::Error)]
pub enum ContextError {
    /// 摘要模型调用或流消费失败。
    #[error("摘要模型调用失败: {0}")]
    Model(#[from] wavecode_llm::LlmError),
    /// 摘要模型未产出任何文本（畸形流 / 空响应）。
    #[error("摘要模型未产出文本")]
    EmptySummary,
}

/// crate 统一 Result 别名。
pub type Result<T> = std::result::Result<T, ContextError>;

/// 压缩策略抽象（SPEC §6：`summarize(history, budget) -> summary`）。
///
/// 策略可替换（本地摘要 / 更强模型摘要等），但触发管线只有一条（core
/// 编排：阈值线 / reactive compact / `/compact` 共用同一入口）。
#[async_trait::async_trait]
pub trait CompactionStrategy: Send + Sync {
    /// 对 `history` 生成结构化摘要；`budget` 为摘要调用的输出 token 预算。
    async fn summarize(&self, history: &[Message], budget: u32) -> Result<String>;
}

/// 摘要请求的 system prompt（与主会话区分；测试 mock 可据此分流脚本）。
const SUMMARY_SYSTEM: &str =
    "You are a context compaction assistant producing structured conversation summaries.";

/// 摘要指令（追加在历史末尾的 user 消息）：五要素标题原样锁定——
/// 目标 / 进展 / 关键决策 / 文件清单 / 待办（SPEC §6 / DEV-PLAN P3 验收锚点）。
const SUMMARY_INSTRUCTION: &str = "\
以上是编程 agent 与用户的对话历史。请压缩为结构化摘要，必须原样包含以下五节标题：
## 目标 —— 用户的总体目标与当前任务
## 进展 —— 已完成的工作、当前进行到哪一步
## 关键决策 —— 已确认的技术选型、方案与约束（含理由）
## 文件清单 —— 已创建 / 修改 / 读取的关键文件路径及其状态
## 待办 —— 尚未完成的事项与下一步
要求：保留具体文件名、路径、命令与错误信息；只输出摘要本身，不要寒暄。";

/// 摘要消息正文的前缀（user 角色的 meta 消息，标注其后历史的口径）。
pub const SUMMARY_MESSAGE_PREFIX: &str = "[上下文压缩] 早前对话已压缩为以下摘要：";

/// 首版压缩策略：用当前模型的一次调用生成五要素结构化摘要。
pub struct ModelSummary {
    model: Arc<dyn ChatModel>,
    model_name: String,
}

impl ModelSummary {
    /// `model` 复用主会话模型通道（压缩与主会话同模型，SPEC §6 首版）。
    pub fn new(model: Arc<dyn ChatModel>, model_name: String) -> Self {
        Self { model, model_name }
    }
}

#[async_trait::async_trait]
impl CompactionStrategy for ModelSummary {
    async fn summarize(&self, history: &[Message], budget: u32) -> Result<String> {
        let mut messages = history.to_vec();
        messages.push(Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: SUMMARY_INSTRUCTION.to_owned(),
            }],
        });
        let req = ChatRequest {
            model: self.model_name.clone(),
            system: SUMMARY_SYSTEM.to_owned(),
            messages: Arc::new(messages),
            tools: Vec::new(),
            max_tokens: budget.max(1),
        };
        let mut stream = self.model.stream(req).await?;
        let mut text = String::new();
        while let Some(item) = stream.next().await {
            if let StreamEvent::TextDelta { text: delta } = item? {
                text.push_str(&delta);
            }
        }
        if text.trim().is_empty() {
            return Err(ContextError::EmptySummary);
        }
        Ok(text)
    }
}

/// 压缩产物。
#[derive(Debug)]
pub struct CompactOutcome {
    /// 压缩后的新历史（摘要消息 + 最近 N 条原文，已 normalize）。
    pub messages: Vec<Message>,
    /// 摘要正文（不含 [`SUMMARY_MESSAGE_PREFIX`]）。
    pub summary: String,
}

/// 压缩管线唯一入口（core 的三类触发共用）：
/// 摘要消息 + 最近 `cfg.keep_recent` 条原文组成新历史。
///
/// 截断边界的配对处理策略（二选一，本实现选**剔除孤儿**）：
/// 从后往前取 N 条时，若窗口首条是 tool_result user 消息，其配对
/// assistant tool_use 已被丢弃——向前扩展到 user 文本边界会把更多原文
/// （往往是大段 tool 输出）留在窗口内，条数不可控，违背压缩目的；改为交
/// 由 [`normalize_history`] 剔除孤儿块，被丢弃部分的信息由摘要承接。
pub async fn compact_history(
    history: &[Message],
    strategy: &dyn CompactionStrategy,
    cfg: &ContextConfig,
) -> Result<CompactOutcome> {
    let summary = strategy.summarize(history, cfg.summary_max_tokens).await?;
    let start = history.len().saturating_sub(cfg.keep_recent);
    let mut messages = Vec::with_capacity(history.len() - start + 1);
    messages.push(Message {
        role: Role::User,
        content: vec![ContentBlock::Text {
            text: format!("{SUMMARY_MESSAGE_PREFIX}\n\n{summary}"),
        }],
    });
    messages.extend_from_slice(&history[start..]);
    let messages = normalize_history(&messages);
    Ok(CompactOutcome { messages, summary })
}

// ---------------------------------------------------------------------------
// 历史 normalize 与配对检查
// ---------------------------------------------------------------------------

/// 孤儿 tool_use 补全结果的回灌文案（is_error，模型可据此重试或放弃）。
const MISSING_RESULT_CONTENT: &str = "tool result unavailable (history normalized)";

/// 历史 normalize（压缩 / 恢复路径共用的独立纯函数，100% 可单测）：
/// 1. 移除空 content 消息（被中断的空消息等，Anthropic 拒绝空 content 数组）；
/// 2. 孤儿 tool_use（assistant 声明了调用但无配对 tool_result）：按声明序补
///    is_error 结果（对齐 Anthropic "tool_use 必有配对 tool_result" 约束）；
/// 3. 孤儿 tool_result（无配对 tool_use，典型来源是压缩截断）：剔除该块，
///    消息变空则整条移除；同一 user 消息中的其他块（文本等）保留。
pub fn normalize_history(history: &[Message]) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::with_capacity(history.len());
    let mut i = 0;
    while i < history.len() {
        let m = &history[i];
        if m.content.is_empty() {
            i += 1; // 规则 1：空 content 消息移除
            continue;
        }
        let tool_use_ids: Vec<&str> = if m.role == Role::Assistant {
            m.content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolUse { id, .. } => Some(id.as_str()),
                    _ => None,
                })
                .collect()
        } else {
            Vec::new()
        };
        if !tool_use_ids.is_empty() {
            out.push(m.clone());
            // 紧随的 user 消息提供配对结果（按声明序逐 id 匹配）。
            let next = history.get(i + 1).filter(|n| n.role == Role::User);
            let mut results = Vec::with_capacity(tool_use_ids.len());
            for id in &tool_use_ids {
                let matched = next.and_then(|n| {
                    n.content.iter().find(
                        |b| matches!(b, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == id),
                    )
                });
                results.push(matched.cloned().unwrap_or(ContentBlock::ToolResult {
                    tool_use_id: (*id).to_owned(),
                    content: MISSING_RESULT_CONTENT.to_owned(),
                    is_error: true,
                }));
            }
            out.push(Message {
                role: Role::User,
                content: results,
            });
            if let Some(n) = next {
                // next 中未消费的块：孤儿 ToolResult 剔除（规则 3），其余保留。
                let rest: Vec<ContentBlock> = n
                    .content
                    .iter()
                    .filter(|b| !matches!(b, ContentBlock::ToolResult { .. }))
                    .cloned()
                    .collect();
                if !rest.is_empty() {
                    out.push(Message {
                        role: Role::User,
                        content: rest,
                    });
                }
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        // 规则 3：user 消息中的孤儿 ToolResult 块剔除，其余块保留。
        if m.role == Role::User
            && m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
        {
            let rest: Vec<ContentBlock> = m
                .content
                .iter()
                .filter(|b| !matches!(b, ContentBlock::ToolResult { .. }))
                .cloned()
                .collect();
            if !rest.is_empty() {
                out.push(Message {
                    role: Role::User,
                    content: rest,
                });
            }
            i += 1;
            continue;
        }
        out.push(m.clone());
        i += 1;
    }
    out
}

/// 配对完整性检查（压缩 / 恢复路径的测试断言复用）：
/// 返回全部违例描述，空 Vec = 配对完整。
///
/// 约束（Anthropic）：assistant 的每个 tool_use 必须在紧随的 user 消息中
/// 有同 id 的 tool_result；user 消息中的每个 tool_result 必须配对前一条
/// assistant 消息中的 tool_use。
pub fn find_pairing_violations(history: &[Message]) -> Vec<String> {
    let mut violations = Vec::new();
    for (i, m) in history.iter().enumerate() {
        match m.role {
            Role::Assistant => {
                let ids: Vec<&str> = m
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolUse { id, .. } => Some(id.as_str()),
                        _ => None,
                    })
                    .collect();
                if ids.is_empty() {
                    continue;
                }
                let next = history.get(i + 1).filter(|n| n.role == Role::User);
                for id in ids {
                    let paired = next.is_some_and(|n| {
                        n.content.iter().any(
                            |b| matches!(b, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == id),
                        )
                    });
                    if !paired {
                        violations.push(format!("消息[{i}] 的 tool_use({id}) 无配对 tool_result"));
                    }
                }
            }
            Role::User => {
                let prev_ids: Vec<&str> = history
                    .get(i.wrapping_sub(1))
                    .filter(|_| i > 0)
                    .filter(|p| p.role == Role::Assistant)
                    .map(|p| {
                        p.content
                            .iter()
                            .filter_map(|b| match b {
                                ContentBlock::ToolUse { id, .. } => Some(id.as_str()),
                                _ => None,
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                for b in &m.content {
                    if let ContentBlock::ToolResult { tool_use_id, .. } = b
                        && !prev_ids.contains(&tool_use_id.as_str())
                    {
                        violations.push(format!(
                            "消息[{i}] 的 tool_result({tool_use_id}) 无配对 tool_use"
                        ));
                    }
                }
            }
        }
    }
    violations
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;
    use std::sync::Mutex;
    use wavecode_llm::Usage;

    fn user_text(text: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: text.to_owned(),
            }],
        }
    }

    fn assistant_text(text: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: text.to_owned(),
            }],
        }
    }

    fn tool_use(id: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: id.to_owned(),
                name: "read_file".to_owned(),
                input: serde_json::json!({"path": "a.txt"}),
            }],
        }
    }

    fn tool_result(id: &str, is_error: bool) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id.to_owned(),
                content: "content".to_owned(),
                is_error,
            }],
        }
    }

    // --- 核算 ---

    #[test]
    fn estimate_tokens_scales_with_chars_and_ratio() {
        let history = vec![user_text(&"x".repeat(400))];
        let est4 = estimate_tokens(&history, 4);
        let est2 = estimate_tokens(&history, 2);
        assert_eq!(est4, 100 + 4, "400 字符 / 4 + 1 条消息结构开销");
        assert_eq!(est2, 200 + 4);
        assert_eq!(estimate_tokens(&[], 4), 0);
        // ratio 防零：按 1 处理不 panic
        assert!(estimate_tokens(&history, 0) > 0);
    }

    // --- 三级阈值边界 ---

    #[test]
    fn threshold_boundaries() {
        let t = Thresholds::default();
        let w = 200_000u64;
        // 警告线：window - 20k = 180_000
        assert_eq!(t.check(179_999, w), BudgetLevel::Ok);
        assert_eq!(t.check(180_000, w), BudgetLevel::Warning);
        // 自动压缩线：window - 13k = 187_000
        assert_eq!(t.check(186_999, w), BudgetLevel::Warning);
        assert_eq!(t.check(187_000, w), BudgetLevel::AutoCompact);
        // 阻塞线：window - 3k = 197_000
        assert_eq!(t.check(196_999, w), BudgetLevel::AutoCompact);
        assert_eq!(t.check(197_000, w), BudgetLevel::Blocking);
        assert_eq!(t.check(200_000, w), BudgetLevel::Blocking);
    }

    #[test]
    fn threshold_saturates_when_window_smaller_than_margin() {
        let t = Thresholds::default();
        // 窗口 2k < blocking_margin 3k：任何占用都阻塞（宁过度压缩不越窗）
        assert_eq!(t.check(1, 2_000), BudgetLevel::Blocking);
        // 窗口 10k：介于 13k 与 3k 之间——警告/自动线压到 0，阻塞线 7k
        assert_eq!(t.check(100, 10_000), BudgetLevel::AutoCompact);
        assert_eq!(t.check(7_000, 10_000), BudgetLevel::Blocking);
    }

    // --- normalize ---

    #[test]
    fn normalize_removes_empty_messages() {
        let history = vec![
            user_text("hi"),
            Message {
                role: Role::Assistant,
                content: vec![],
            },
            assistant_text("hello"),
        ];
        let out = normalize_history(&history);
        assert_eq!(out.len(), 2);
        assert!(find_pairing_violations(&out).is_empty());
    }

    #[test]
    fn normalize_completes_orphan_tool_use_with_error_result() {
        // assistant 声明了两个调用，user 只回了一个，且夹带文本
        let history = vec![
            user_text("干活"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::ToolUse {
                        id: "t1".into(),
                        name: "read_file".into(),
                        input: serde_json::json!({}),
                    },
                    ContentBlock::ToolUse {
                        id: "t2".into(),
                        name: "read_file".into(),
                        input: serde_json::json!({}),
                    },
                ],
            },
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "t1".into(),
                        content: "A".into(),
                        is_error: false,
                    },
                    ContentBlock::Text {
                        text: "补充说明".into(),
                    },
                ],
            },
        ];
        let out = normalize_history(&history);
        assert!(find_pairing_violations(&out).is_empty());
        // 配对消息：t1 用原结果，t2 补 is_error
        let pair = &out[2];
        assert_eq!(pair.role, Role::User);
        let contents: Vec<(&str, bool)> = pair
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolResult {
                    tool_use_id,
                    is_error,
                    ..
                } => Some((tool_use_id.as_str(), *is_error)),
                _ => None,
            })
            .collect();
        assert_eq!(contents, vec![("t1", false), ("t2", true)]);
        // 文本块保留为独立 user 消息
        assert!(matches!(&out[3].content[0], ContentBlock::Text { text } if text == "补充说明"));
    }

    #[test]
    fn normalize_drops_orphan_tool_results() {
        let history = vec![
            tool_result("ghost", false), // 无配对 tool_use（如压缩截断的窗口头）
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "ghost2".into(),
                        content: "x".into(),
                        is_error: false,
                    },
                    ContentBlock::Text {
                        text: "保留文本".into(),
                    },
                ],
            },
            user_text("hello"),
        ];
        let out = normalize_history(&history);
        assert!(find_pairing_violations(&out).is_empty());
        assert_eq!(out.len(), 2, "整条孤儿消息移除、混合消息只剩文本块");
        assert!(matches!(&out[0].content[0], ContentBlock::Text { text } if text == "保留文本"));
    }

    #[test]
    fn normalize_is_idempotent_on_wellformed_history() {
        let history = vec![
            user_text("读文件"),
            tool_use("t1"),
            tool_result("t1", false),
            assistant_text("读完了"),
        ];
        let out = normalize_history(&history);
        assert_eq!(out, history, "配对完整的历史原样通过");
        assert!(find_pairing_violations(&out).is_empty());
    }

    // --- 压缩（含信息保留率验收锚点） ---

    /// 脚本化 mock：回放预排事件序列。
    struct MockModel {
        scripts: Vec<Vec<StreamEvent>>,
        calls: Mutex<u32>,
    }

    #[async_trait::async_trait]
    impl ChatModel for MockModel {
        async fn stream(
            &self,
            req: ChatRequest,
        ) -> wavecode_llm::Result<wavecode_llm::EventStream> {
            // 摘要请求断言：历史完整透传 + 五要素指令 + 无工具 + 预算生效
            assert_eq!(req.system, SUMMARY_SYSTEM);
            assert!(req.tools.is_empty());
            assert_eq!(req.max_tokens, 777);
            let last = req.messages.last().expect("摘要指令已追加");
            assert!(
                matches!(&last.content[0], ContentBlock::Text { text } if text.contains("## 目标") && text.contains("## 待办"))
            );
            let mut n = self.calls.lock().unwrap();
            let idx = (*n as usize).min(self.scripts.len() - 1);
            *n += 1;
            Ok(Box::pin(stream::iter(
                self.scripts[idx].clone().into_iter().map(Ok),
            )))
        }
    }

    /// 含五要素的脚本化摘要响应。
    fn scripted_summary() -> Vec<StreamEvent> {
        let summary = "\
## 目标
搭建电商平台后端（Rust workspace）。
## 进展
已完成购物车服务与订单骨架，库存扣减进行中。
## 关键决策
选用 SQLite 落地首版（理由：零运维）；支付走 mock gateway。
## 文件清单
crates/shop/src/cart.rs（已创建）；crates/shop/src/order.rs（已修改）。
## 待办
库存并发扣减测试；接入结算流水。";
        vec![
            StreamEvent::TextDelta {
                text: summary.into(),
            },
            StreamEvent::MessageComplete {
                stop_reason: "end_turn".into(),
                usage: Usage {
                    input_tokens: 5000,
                    output_tokens: 120,
                },
            },
        ]
    }

    /// 构造含五要素信息 + 工具配对的长会话历史（16 条）。
    fn long_history() -> Vec<Message> {
        let mut h = vec![
            user_text("目标：搭建电商平台后端，先做购物车。"),
            assistant_text("关键决策：首版用 SQLite，零运维。"),
            tool_use("t1"),
            tool_result("t1", false),
            assistant_text("已创建 crates/shop/src/cart.rs。"),
            tool_use("t2"),
            tool_result("t2", false),
            assistant_text("订单骨架完成，待办：库存并发扣减测试。"),
        ];
        // 补足长度（>keep_recent），内容与五要素无关的中间过程
        for i in 0..8 {
            h.push(user_text(&format!("中间过程 {i}")));
        }
        h
    }

    /// P3 验收锚点：压缩信息保留率——摘要逐项含五要素，最近 N 条原文保留。
    #[tokio::test]
    async fn compact_retains_five_elements_and_recent_tail() {
        let history = long_history();
        let tail_start = history.len() - 4;
        let model = Arc::new(MockModel {
            scripts: vec![scripted_summary()],
            calls: Mutex::new(0),
        });
        let strategy = ModelSummary::new(model, "mock".into());
        let cfg = ContextConfig {
            keep_recent: 4,
            summary_max_tokens: 777,
            ..Default::default()
        };
        let outcome = compact_history(&history, &strategy, &cfg).await.unwrap();

        // 摘要消息在首位（user meta），逐项含五要素
        let first = &outcome.messages[0];
        assert_eq!(first.role, Role::User);
        let ContentBlock::Text { text } = &first.content[0] else {
            panic!("首条应为摘要文本消息")
        };
        assert!(text.starts_with(SUMMARY_MESSAGE_PREFIX));
        for element in ["目标", "进展", "关键决策", "文件清单", "待办"] {
            assert!(text.contains(element), "摘要缺要素「{element}」: {text}");
        }

        // 最近 4 条原文完整保留（与源历史逐条相等）
        assert_eq!(outcome.messages.len(), 1 + 4);
        assert_eq!(
            &outcome.messages[1..],
            &history[tail_start..],
            "最近 N 条原文应逐条保留"
        );

        // 配对完整性（验收锚点复用断言函数）
        assert_eq!(
            find_pairing_violations(&outcome.messages),
            Vec::<String>::new()
        );
    }

    /// 截断边界：窗口首条是 tool_result（配对 assistant 被丢弃）→ 孤儿剔除。
    #[tokio::test]
    async fn compact_drops_orphan_at_cut_boundary() {
        let mut history = vec![
            user_text("开头"),
            tool_use("t9"),
            tool_result("t9", false), // keep_recent=2 时它会成为窗口首条
            assistant_text("结尾"),
        ];
        history.extend_from_slice(&[user_text("最后")]);
        let model = Arc::new(MockModel {
            scripts: vec![scripted_summary()],
            calls: Mutex::new(0),
        });
        let strategy = ModelSummary::new(model, "mock".into());
        let cfg = ContextConfig {
            keep_recent: 2,
            summary_max_tokens: 777,
            ..Default::default()
        };
        // 窗口 = [tool_result("t9"), assistant_text("结尾")]——首条孤儿
        let outcome = compact_history(&history[..4], &strategy, &cfg)
            .await
            .unwrap();
        assert_eq!(
            find_pairing_violations(&outcome.messages),
            Vec::<String>::new(),
            "压缩后历史不得有孤儿: {:?}",
            outcome.messages
        );
        assert!(
            !outcome.messages.iter().any(|m| m.content.iter().any(
                |b| matches!(b, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "t9")
            )),
            "孤儿 tool_result 应被剔除"
        );
    }

    /// 摘要模型空响应 → EmptySummary 错误（不得静默以空摘要替换历史）。
    #[tokio::test]
    async fn empty_summary_is_an_error() {
        let model = Arc::new(MockModel {
            scripts: vec![vec![StreamEvent::MessageComplete {
                stop_reason: "end_turn".into(),
                usage: Usage::default(),
            }]],
            calls: Mutex::new(0),
        });
        let strategy = ModelSummary::new(model, "mock".into());
        let result = strategy.summarize(&long_history(), 777).await;
        assert!(matches!(result, Err(ContextError::EmptySummary)));
    }
}
