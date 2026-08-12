//! 记忆系统的 core 侧编排（P6，SPEC §7）。
//!
//! memory crate 无 workspace 内依赖（SPEC §3 依赖矩阵），凡涉及 tools /
//! llm 的能力都落在 core（core→memory 边由矩阵允许）：
//!
//! - [`MemoryWrite`]：`memory_write` 工具——持久记忆条目的模型写入面；
//! - [`MemorySessionConfig`]：装配层（cli bootstrap）注入
//!   [`crate::session::SessionConfig`] 的记忆面——指令记忆拼接产物 /
//!   索引快照 / 存储根；
//! - 自动提取：会话结束派生子代理从会话历史提炼候选条目（简化首版，
//!   整合门控留常量，见下）。
//!
//! **审批挂接**（对齐 sandbox 既有方式，无特判）：`memory_write` 声明
//! `is_read_only() = false`，sandbox `decide()` 的模式默认策略即给出
//! default 模式 `Ask` / plan 模式 `Deny` / bypassPermissions 放行——
//! 与 write_file 同一管道（ApprovalRequested → ExecApproval）。allow/deny
//! 规则以 `command` / `path` 候选键匹配输入，memory_write 的参数
//! （category/content）无此二键，规则天然不命中（同 todo_write 的形态，
//! 但 memory_write **不享** session 内状态豁免——它写真实文件）。
//!
//! **首版简化（诚实声明）**：SPEC §7.2 的记忆整合（门控触发、合并重复、
//! 剔除失效、精简索引）未实现——提取为纯追加式；门控参数留
//! [`CONSOLIDATION_MIN_INTERVAL_HOURS`] / [`CONSOLIDATION_MIN_NEW_SESSIONS`]
//! 常量备后续落地。索引注入取启动时快照：会话内新写入的条目下一会话
//! 才进入注入——保持系统提示词前缀字节稳定（prompt cache，SPEC §5.4）。

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Value, json};
use wavecode_llm::{ContentBlock, Message, Role};
use wavecode_protocol::SubagentStatus;
use wavecode_tools::{Tool, ToolCtx, ToolOutput};

use crate::subagent::{SubagentManager, SubagentType, TaskSpec};

// memory crate 的公开面经 core 再导出：cli 装配层（bootstrap 收集
// WAVECODE.md 与索引、`/memory` 命令）不新增 cli→memory 依赖边
//（SPEC §3 矩阵 cli 行无 memory；core 行本已允许）。
pub use wavecode_memory::{
    INDEX_FILE, InstructionMemory, MAX_INCLUDE_DEPTH, MemoryCategory, MemoryStore,
    collect as collect_instruction_memory, find_project_root, home_dir, parse_extracted_entries,
};

/// SPEC §7.2 原设计的整合门控：距上次整合 ≥24h 且期间 ≥5 个新会话才
/// 触发整合。首版简化为纯追加式提取、不整合——参数留常量备后续落地。
#[allow(dead_code)]
pub const CONSOLIDATION_MIN_INTERVAL_HOURS: u64 = 24;
/// 见 [`CONSOLIDATION_MIN_INTERVAL_HOURS`]。
#[allow(dead_code)]
pub const CONSOLIDATION_MIN_NEW_SESSIONS: u64 = 5;

/// 提取输入的摘要上限：单条消息文本截断 500 字符，总量上限 20k（字节
/// 口径，近似即可——上限只为防提取输入爆炸）。
const DIGEST_ENTRY_MAX_CHARS: usize = 500;
const DIGEST_TOTAL_MAX_CHARS: usize = 20_000;

/// 装配层注入的记忆面（`SessionConfig.memory`；None = 无记忆能力）。
#[derive(Debug, Clone)]
pub struct MemorySessionConfig {
    /// 指令记忆拼接产物（WAVECODE.md 收集结果，注入系统提示词槽位）。
    pub instruction_memory: String,
    /// 持久记忆索引快照（启动时的 MEMORY.md 内容，注入系统提示词槽位）。
    /// 快照纪律：会话内 memory_write 的新条目下一会话才进注入——保持
    /// 系统提示词前缀字节稳定（SPEC §5.4）。
    pub memory_index: String,
    /// 持久记忆存储根（memory_write 与自动提取的写入目标；生产为
    /// `~/.wavecode/memories/`，测试注入 tempfile）。
    pub store_root: PathBuf,
}

/// `memory_write` 工具：追加一条持久记忆（类别文件 + MEMORY.md 索引）。
///
/// 写入需审批——经 sandbox 非只读默认策略挂接（见模块级注释），
/// 本工具自身不做任何权限判断。
pub struct MemoryWrite {
    store: MemoryStore,
}

impl MemoryWrite {
    /// 以存储根构造（`Session::new` 按 `SessionConfig.memory` 装配）。
    pub fn new(store_root: PathBuf) -> Self {
        Self {
            store: MemoryStore::new(store_root),
        }
    }
}

#[async_trait::async_trait]
impl Tool for MemoryWrite {
    fn name(&self) -> &str {
        "memory_write"
    }

    fn description(&self) -> &str {
        "Save a durable memory entry that persists across sessions. Categories: 'user' (facts \
         about the user's preferences, habits or role), 'feedback' (explicit user corrections or \
         guidance to follow from now on), 'project' (project-specific conventions, decisions, \
         environment facts), 'reference' (pointers to external docs or resources). The entry is \
         appended to the category file and the MEMORY.md index; writing requires user approval."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "category": {
                    "type": "string",
                    "enum": ["user", "feedback", "project", "reference"],
                    "description": "Memory category"
                },
                "content": {
                    "type": "string",
                    "description": "The memory entry content (markdown; multi-line allowed)"
                }
            },
            "required": ["category", "content"]
        })
    }

    fn is_read_only(&self) -> bool {
        // 写真实文件（用户记忆目录）：非只读 → 进串行段过审批门（模块注释）。
        false
    }

    async fn execute(&self, input: Value, _ctx: &ToolCtx) -> wavecode_tools::Result<ToolOutput> {
        let err = |reason: String| {
            Ok(ToolOutput {
                content: reason,
                is_error: true,
            })
        };
        let category = match input.get("category").and_then(Value::as_str) {
            Some(raw) => match MemoryCategory::parse(raw) {
                Some(c) => c,
                None => {
                    return err(format!(
                        "invalid category {raw:?} (expected user | feedback | project | reference)"
                    ));
                }
            },
            None => {
                return err("missing parameter 'category' (string required)".to_owned());
            }
        };
        let content = match input.get("content").and_then(Value::as_str) {
            Some(s) if !s.trim().is_empty() => s.to_owned(),
            _ => {
                return err(
                    "missing or invalid parameter 'content' (non-empty string required)".to_owned(),
                );
            }
        };
        // 小文件追加为同步 IO（memory crate 无 tokio 依赖）：包
        // spawn_blocking 避免阻塞执行器（对齐 §19.3 纪律）。
        let store = self.store.clone();
        let file_name = category.file_name();
        tokio::task::spawn_blocking(move || store.append(category, &content))
            .await
            .map_err(|e| wavecode_tools::ToolsError::Io(std::io::Error::other(e)))??;
        Ok(ToolOutput {
            content: format!(
                "Memory saved to [{cat}] (appended to {file_name} and indexed in MEMORY.md).\n\
                 Note: it will be injected into the system prompt from the next session.",
                cat = category.as_str(),
            ),
            is_error: false,
        })
    }
}

/// 提取子代理的输出格式约定（与
/// [`wavecode_memory::parse_extracted_entries`] 的解析面一一对应）。
const EXTRACTION_PREAMBLE: &str = "\
You are a memory extraction agent. Read the conversation below and extract durable memory \
candidates worth keeping across sessions.
Categories:
- [user]: facts about the user (preferences, habits, role)
- [feedback]: explicit corrections or guidance from the user (\"do/don't ... from now on\")
- [project]: project-specific facts (repo conventions, architecture decisions, environment facts)
- [reference]: pointers to external resources (docs, links, external systems)
Output ONLY entries in this exact format (the tag and content may share a line; multi-line \
content continues until the next tag):
[user] ...
[project] ...
If nothing is worth remembering, output exactly: NONE
Do not include any other commentary.";

/// 会话历史 → 提取输入摘要：逐消息 `role: 文本`（tool_use 以 `[tool: name]`
/// 标记，tool_result 内容省略——体量与噪声控制），单条截断 500、总量
/// 上限 20k 字符。
pub fn build_history_digest(messages: &[Message]) -> String {
    let mut out = String::new();
    for m in messages {
        let role = match m.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        let mut body = String::new();
        for block in &m.content {
            match block {
                ContentBlock::Text { text } => {
                    body.push_str(text);
                    body.push('\n');
                }
                ContentBlock::ToolUse { name, .. } => {
                    body.push_str(&format!("[tool: {name}]\n"));
                }
                ContentBlock::ToolResult { .. } => {}
            }
        }
        let body = body.trim();
        if body.is_empty() {
            continue;
        }
        let truncated: String = body.chars().take(DIGEST_ENTRY_MAX_CHARS).collect();
        out.push_str(role);
        out.push_str(": ");
        out.push_str(&truncated);
        out.push('\n');
        if out.len() >= DIGEST_TOTAL_MAX_CHARS {
            out.push_str("...(truncated)\n");
            break;
        }
    }
    out
}

/// 提炼一轮（Session 提取入口的共享实现）：派生同步子代理（explore 类型
/// ——只读工具面，提取不需要写工具）读历史摘要、按约定格式输出候选条目，
/// 解析后追加到存储。返回写入条数；子代理失败以 Err 上抛（调用方决定
/// 静默策略）。
pub async fn extract_with_manager(
    mgr: Arc<SubagentManager>,
    history: Arc<Vec<Message>>,
    store_root: PathBuf,
) -> anyhow::Result<usize> {
    let digest = build_history_digest(&history);
    let spec = TaskSpec {
        description: "提炼会话记忆".into(),
        prompt: format!("{EXTRACTION_PREAMBLE}\n\n<conversation>\n{digest}\n</conversation>"),
        subagent_type: SubagentType::Explore,
        preamble: None,
        allowed_tools: None,
    };
    let result = mgr.run_sync(spec).await;
    if result.status == SubagentStatus::Failed {
        anyhow::bail!("提取子代理失败: {}", result.summary);
    }
    let entries = parse_extracted_entries(&result.summary);
    if entries.is_empty() {
        return Ok(0);
    }
    let count = entries.len();
    // 与 MemoryWrite::execute 同理：同步小文件 IO 包 spawn_blocking。
    tokio::task::spawn_blocking(move || {
        let store = MemoryStore::new(store_root);
        for (category, content) in entries {
            store.append(category, &content)?;
        }
        std::io::Result::Ok(())
    })
    .await??;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_msg(role: Role, text: &str) -> Message {
        Message {
            role,
            content: vec![ContentBlock::Text {
                text: text.to_owned(),
            }],
        }
    }

    /// 摘要形态：role 前缀、tool_use 标记、tool_result 省略、空消息跳过。
    #[test]
    fn digest_shapes() {
        let messages = vec![
            text_msg(Role::User, "帮我看下构建"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::ToolUse {
                        id: "t1".into(),
                        name: "shell".into(),
                        input: json!({}),
                    },
                    ContentBlock::Text {
                        text: "构建通过".into(),
                    },
                ],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "大段工具输出".into(),
                    is_error: false,
                }],
            },
            text_msg(Role::Assistant, ""),
        ];
        let digest = build_history_digest(&messages);
        assert!(digest.contains("user: 帮我看下构建"));
        assert!(digest.contains("assistant: [tool: shell]\n构建通过"));
        assert!(!digest.contains("大段工具输出"), "tool_result 内容省略");
    }

    /// 单条截断：超长文本按 500 字符截断。
    #[test]
    fn digest_truncates_long_entry() {
        let long = "x".repeat(2000);
        let digest = build_history_digest(&[text_msg(Role::User, &long)]);
        // "user: " + 500 字符 + "\n"
        assert_eq!(digest.len(), 6 + 500 + 1);
    }
}
