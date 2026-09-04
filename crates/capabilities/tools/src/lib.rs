//! wavecode-tools — 工具框架与内置工具集。
//!
//! 形态：[`Tool`] trait、[`Registry`] 注册表，内置文件工具
//! （`read_file` / `write_file` / `edit_file` / `list_dir`）、检索工具
//! （`grep` / `glob`）、`shell` 工具与 session 任务清单工具（`todo_write`，
//! P4 deepagents planning）。文件与检索工具经 `path_guard`
//! 把所有路径约束在 [`ToolCtx::cwd`] 之下，防 `..` 越界与绝对路径逃逸。
//! 内置工具的 execute 均为真 async（`tokio::fs` / `tokio::process`；
//! grep/glob 的目录遍历为同步 API，内部包 `spawn_blocking`）。后续里程碑
//! 追加 web / 浏览器等工具，执行管道（schema 校验、hook、权限审批）由 core 编排。

mod fs;
mod path_guard;
mod search;
mod shell_tool;
mod todo_tool;

pub use todo_tool::{TodoItem, TodoStatus, TodoStore, TodoWrite, format_todos};

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};

/// 锁中毒的统一恢复策略（本 crate 单点决策）：这些锁保护的都是单操作
/// 临界区（一次 insert / 赋值 / 读取），持锁期间 panic 不会留下半截
/// 不变量——中毒时取回守卫继续执行（panic 已沿原线程传播），不做
/// 二次 panic 级联。
pub(crate) fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// [`lock`] 的 RwLock 读侧同例。
pub(crate) fn read<T>(l: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(|e| e.into_inner())
}

/// [`lock`] 的 RwLock 写侧同例。
pub(crate) fn write<T>(l: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(|e| e.into_inner())
}

/// 工具执行上下文。
#[derive(Debug, Clone)]
pub struct ToolCtx {
    /// 工作目录（约定为绝对路径）：文件工具的所有路径都约束在其之下。
    pub cwd: std::path::PathBuf,
    /// spawn 子进程前要剔除的环境变量名（由装配层注入 provider 的
    /// `env_key` 等显式名单；shell 工具另按敏感后缀模式自动剔除，
    /// 见 `shell_tool::sanitize_env`）。
    pub deny_env: Vec<String>,
}

/// 工具输出。
///
/// `is_error = true` 表示业务失败（文件不存在、匹配不唯一、参数缺失等），
/// `content` 为人类可读原因——会回灌给模型，供其自我纠正。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

/// 工具抽象。
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    /// 工具名（注入模型，全局唯一）。返回 `&str` 而非 `&'static str`：
    /// M3 MCP 动态工具需运行时命名。
    fn name(&self) -> &str;
    /// 能力描述（英文，供模型消费）。
    fn description(&self) -> &str;
    /// 参数的 JSON Schema（注入采样请求）。
    fn input_schema(&self) -> serde_json::Value;
    /// 只读工具可并行执行；写入类工具需串行。
    fn is_read_only(&self) -> bool;
    /// 破坏性工具（删除、覆盖不可恢复状态等）默认需审批（SPEC §11.1；
    /// P2 sandbox/HITL 接线，P1 仅落地 trait 面）。默认非破坏性。
    fn is_destructive(&self) -> bool {
        false
    }
    /// 执行前语义校验（JSON Schema 之外的检查，SPEC §11.1 执行管道的一环；
    /// P2 起由 core 编排调用）。默认实现直接放行。
    async fn validate(&self, _input: &serde_json::Value) -> Result<()> {
        Ok(())
    }
    /// 执行。`Err` 仅用于实现级故障（io 错误等）；业务失败返回
    /// `Ok(ToolOutput { is_error: true, .. })`，不得 panic。
    ///
    /// 内置工具均为真 async：文件工具用 `tokio::fs`，shell 用
    /// `tokio::process`；grep/glob 的目录遍历是 `glob` crate 同步 API，
    /// 工具内部包 `spawn_blocking` 自理（SPEC §19.3）。
    async fn execute(&self, input: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput>;
}

/// tools crate 统一错误类型。
#[derive(Debug, thiserror::Error)]
pub enum ToolsError {
    /// IO 层错误。
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),
    /// 输入无效（如空路径）。
    #[error("无效输入: {message}")]
    InvalidInput { message: String },
    /// 路径逃逸出工作目录。
    #[error("路径逃逸工作目录: {path}")]
    PathEscape { path: String },
}

/// crate 内统一 Result 别名。
pub type Result<T> = std::result::Result<T, ToolsError>;

/// 工具面白名单的共享句柄（P7 skills `allowed-tools`，SPEC §8.2）：
/// `Some(set)` 时仅 set 内工具可执行，`None` 不限。
///
/// 与 [`TodoStore`] 同形态——由**会话配置**持有（per-session），而非
/// [`Registry`]（只做工具索引）或 [`ToolCtx`]（每 turn 重建的纯数据快照）。
/// core 执行管道在 PreToolUse / sandbox 判定前检查；skill 工具激活时写入。
/// 语义为 **turn 级**：core 在每个 turn 入口清零（首版取舍，见 core::skills）。
#[derive(Clone, Default)]
pub struct ToolAllowlist {
    inner: Arc<Mutex<Option<HashSet<String>>>>,
}

impl ToolAllowlist {
    /// 设置白名单（None = 解除限制）。
    pub fn set(&self, names: Option<HashSet<String>>) {
        *lock(&self.inner) = names;
    }

    /// 工具是否可执行（无白名单 → true）。
    pub fn is_allowed(&self, name: &str) -> bool {
        lock(&self.inner)
            .as_ref()
            .is_none_or(|set| set.contains(name))
    }
}

/// 工具注册表：按名索引，供执行管道查找与生成请求侧 `ToolSpec` 清单。
///
/// **不含** session 级 planning / skills 状态（[`TodoStore`] / [`ToolAllowlist`]）
/// ——那些由会话配置持有，避免 Registry 成为跨业务变化中心。
pub struct Registry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl Registry {
    /// 注册内置工具（文件 / 检索 / shell），**不含** `todo_write`。
    ///
    /// `todo_write` 须由调用方经 [`Registry::with_todo_write`] 注入，并与
    /// 会话配置中的 [`TodoStore`] 共享同一句柄。
    pub fn builtin() -> Self {
        let mut reg = Self {
            tools: HashMap::new(),
        };
        reg.register(Arc::new(fs::ReadFile));
        reg.register(Arc::new(fs::WriteFile));
        reg.register(Arc::new(fs::EditFile));
        reg.register(Arc::new(fs::ListDir));
        reg.register(Arc::new(search::Grep));
        reg.register(Arc::new(search::Glob));
        reg.register(Arc::new(shell_tool::Shell));
        reg
    }

    /// 注册 `todo_write`，与会话级 [`TodoStore`] 共享状态。
    pub fn with_todo_write(mut self, todos: TodoStore) -> Self {
        self.register(Arc::new(TodoWrite::new(todos)));
        self
    }

    /// 完整内置集（含 `todo_write`）与配套 [`TodoStore`]——会话装配应
    /// 把返回的 store 写入会话配置，保证工具与 steering 同源。
    pub fn builtin_with_todos() -> (Self, TodoStore) {
        let todos = TodoStore::default();
        (Self::builtin().with_todo_write(todos.clone()), todos)
    }

    /// 派生按名白名单子集注册表（P7 skill fork 的 `allowed-tools` 工具面）：
    /// 仅保留名单内的工具（未知名静默略过——名单来自用户 frontmatter，
    /// 拼错的代价是该工具不可用，影响面局限于该 skill）。
    pub fn name_subset(&self, names: &[String]) -> Self {
        let mut reg = Self {
            tools: HashMap::new(),
        };
        for name in names {
            if let Some(tool) = self.tools.get(name) {
                reg.register(tool.clone());
            }
        }
        reg
    }

    /// 派生只读子集注册表（P5 explore 类型子代理的工具面）：仅保留
    /// `is_read_only()` 的工具（`todo_write` 非只读，不会进入子集）。
    pub fn read_only_subset(&self) -> Self {
        let mut reg = Self {
            tools: HashMap::new(),
        };
        for tool in self.tools.values() {
            if tool.is_read_only() {
                reg.register(tool.clone());
            }
        }
        reg
    }

    /// 注册工具（M3 起 MCP 等动态工具也经此注册）。
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_owned(), tool);
    }

    /// 全部工具的 `ToolSpec`，按 name 排序，输出稳定。
    pub fn specs(&self) -> Vec<wavecode_llm::ToolSpec> {
        let mut tools: Vec<&Arc<dyn Tool>> = self.tools.values().collect();
        tools.sort_by_key(|t| t.name());
        tools
            .iter()
            .map(|t| wavecode_llm::ToolSpec {
                name: t.name().to_owned(),
                description: t.description().to_owned(),
                input_schema: t.input_schema(),
            })
            .collect()
    }

    /// 按名查找工具。
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P7：白名单默认不限；set 后仅名单内可执行；解除后恢复。
    #[test]
    fn allowlist_gating() {
        let allowlist = ToolAllowlist::default();
        assert!(allowlist.is_allowed("shell"));
        allowlist.set(Some(HashSet::from(["read_file".to_owned()])));
        assert!(allowlist.is_allowed("read_file"));
        assert!(!allowlist.is_allowed("shell"));
        allowlist.set(None);
        assert!(allowlist.is_allowed("shell"));
    }

    /// P7：name_subset 按名过滤；未知名静默略过；只读/写工具按名单保留。
    #[test]
    fn name_subset_filters_by_name() {
        let (reg, _todos) = Registry::builtin_with_todos();
        let sub = reg.name_subset(&["read_file".to_owned(), "grep".to_owned(), "nope".to_owned()]);
        assert!(sub.get("read_file").is_some());
        assert!(sub.get("grep").is_some());
        assert!(sub.get("write_file").is_none());
        assert!(sub.get("shell").is_none());
        assert!(sub.get("todo_write").is_none(), "未列入名单即不可用");
        // specs 输出稳定（按名排序）。
        let names: Vec<String> = sub.specs().into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["grep", "read_file"]);
    }

    /// 会话装配契约：`name_subset`（skill fork 工具面派生）保留同一
    /// `todo_write` 实例——经子集工具写入的状态必须对会话配置持有的
    /// [`TodoStore`] 可见（同 Arc 句柄），否则 todo_write 与 steering /
    /// 上下文注入各写各读、静默失配。
    #[tokio::test]
    async fn todo_write_via_name_subset_shares_session_store() {
        let (reg, todos) = Registry::builtin_with_todos();
        let sub = reg.name_subset(&["todo_write".to_owned()]);
        let tool = sub.get("todo_write").expect("名单内 todo_write 应保留");
        let ctx = ToolCtx {
            cwd: std::env::temp_dir(),
            deny_env: Vec::new(),
        };
        let out = tool
            .execute(
                serde_json::json!({"todos": [{"content": "a", "status": "in_progress"}]}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(todos.snapshot().len(), 1, "子集工具与会话 store 须同句柄");
    }
}
