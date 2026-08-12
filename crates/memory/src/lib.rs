//! wavecode-memory — 记忆系统（P6，SPEC §7）。
//!
//! - 指令记忆（[`instructions`]）：`WAVECODE.md` 分层发现（用户级 → 项目根
//!   → cwd），支持 `@path` 递归引用（深度上限 5，防环）与
//!   `.wavecode/rules/*.md` 规则目录并入；
//! - 持久记忆（[`store`]）：user / feedback / project / reference 四类
//!   条目文件 + `MEMORY.md` 索引；根目录可注入（生产为
//!   `~/.wavecode/memories/`，测试用 tempfile）；
//! - 自动提取产出解析（[`extract`]）：会话结束提取子代理的线格式输出 →
//!   （类别， 内容） 列表。
//!
//! 本 crate 无 workspace 内依赖（SPEC §3 依赖矩阵）：`memory_write` 工具、
//! 审批挂接、提示词注入与提取编排均在 core 侧（core→memory 边由矩阵允许）。
//!
//! 首版简化（诚实声明）：SPEC §7.2 的记忆**整合**（距上次 ≥24h 且 ≥5 个
//! 新会话的门控触发、合并重复条目、剔除失效内容、精简索引）未实现——
//! 首版为纯追加式，门控参数留 core 侧配置常量；`WAVECODE.override.md`
//! 覆盖与 fallback 文件名（CLAUDE.md/AGENTS.md）同留待后续。

pub mod extract;
pub mod instructions;
pub mod store;

pub use extract::parse_extracted_entries;
pub use instructions::{InstructionMemory, MAX_INCLUDE_DEPTH, collect, find_project_root};
pub use store::{INDEX_FILE, MemoryCategory, MemoryStore, home_dir};
