//! `todo_write` 工具：session 级任务清单（deepagents `write_todos` 语义，
//! SPEC §11.2）。
//!
//! 语义：**整体重写**（不是增量 patch）——模型每次调用给出完整清单，
//! 全量替换 session 级共享状态。`id` 按清单位置自动生成（`"1"..="n"`），
//! 调用方不维护 id：对齐 deepagents 无 id 的形态，减少模型负担与 id 漂移；
//! 清单整体重写时位置即身份。
//!
//! 状态共享方案（最小侵入）：清单是 session 级状态，而 [`ToolCtx`] 是纯数据
//! 快照（每 turn 重建），不适合携带可变句柄——共享状态改由 [`crate::Registry`]
//! 持有（Registry 本就是 per-session 装配），[`TodoWrite`] 实例与 Session 经
//! [`crate::Registry::todos`] 拿到同一个 `Arc` 句柄，ToolCtx 保持纯数据不变。

use std::sync::{Arc, RwLock};

use serde_json::{Value, json};

use crate::{Result, Tool, ToolCtx, ToolOutput};

/// 清单条目上限：防模型失控写入超长清单污染上下文与回显输出。
const MAX_TODOS: usize = 100;

/// 清单条目状态（线型三态，对齐 deepagents）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

impl TodoStatus {
    /// 解析状态词；非法值返回 None（调用方转业务失败输出回给模型）。
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "pending" => Some(Self::Pending),
            "in_progress" => Some(Self::InProgress),
            "completed" => Some(Self::Completed),
            _ => None,
        }
    }

    /// 状态词（回显与注入文本用）。
    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
        }
    }
}

/// 清单条目。`id` 由工具按位置生成（见模块注释），调用方只给 content/status。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoItem {
    pub id: String,
    pub content: String,
    pub status: TodoStatus,
}

/// session 级任务清单的共享句柄（`Arc<RwLock<..>>`，克隆即共享）。
///
/// 用 `std::sync::RwLock` 而非 tokio 版：临界区只有 Vec 读写，无跨 await
/// 持锁，同步锁更简（与 `ApprovalGate` 的 Mutex 选型同理）。
#[derive(Debug, Clone, Default)]
pub struct TodoStore {
    inner: Arc<RwLock<Vec<TodoItem>>>,
}

impl TodoStore {
    /// 整体重写清单（todo_write 语义）。
    pub fn write(&self, items: Vec<TodoItem>) {
        *self.inner.write().expect("清单锁中毒即进程已有 panic") = items;
    }

    /// 当前清单快照。
    pub fn snapshot(&self) -> Vec<TodoItem> {
        self.inner
            .read()
            .expect("清单锁中毒即进程已有 panic")
            .clone()
    }

    /// 未完成项计数 `(pending, in_progress)`（stop steering 判据，P4）。
    pub fn unfinished(&self) -> (usize, usize) {
        let items = self.inner.read().expect("清单锁中毒即进程已有 panic");
        let pending = items
            .iter()
            .filter(|i| i.status == TodoStatus::Pending)
            .count();
        let in_progress = items
            .iter()
            .filter(|i| i.status == TodoStatus::InProgress)
            .count();
        (pending, in_progress)
    }
}

/// 清单的编号回显（工具输出 / 上下文注入 / steering 提醒共用）：
/// `1. [pending] 内容`，每行一条。空清单返回固定文案。
pub fn format_todos(items: &[TodoItem]) -> String {
    if items.is_empty() {
        return "Task list is empty.".to_owned();
    }
    items
        .iter()
        .map(|i| format!("{}. [{}] {}", i.id, i.status.label(), i.content))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `todo_write`：整体重写 session 任务清单（deepagents planning）。
pub struct TodoWrite {
    store: TodoStore,
}

impl TodoWrite {
    /// 以共享状态句柄构造（Registry 装配时与 Session 共享同一 Arc）。
    pub fn new(store: TodoStore) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl Tool for TodoWrite {
    fn name(&self) -> &str {
        "todo_write"
    }

    fn description(&self) -> &str {
        "Replace the session task list (full rewrite, not a patch). Use it to break down \
         complex or multi-step tasks before starting, and to update item status \
         (pending / in_progress / completed) as you make progress. Always provide the \
         complete list; an empty array clears it. The updated list is echoed back."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "description": "The complete new task list (replaces the current one)",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {
                                "type": "string",
                                "description": "Task description"
                            },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed"],
                                "description": "Task status"
                            }
                        },
                        "required": ["content", "status"]
                    }
                }
            },
            "required": ["todos"]
        })
    }

    fn is_read_only(&self) -> bool {
        // 改写 session 状态（非只读语义真相）：进串行段执行，防一批内多次
        // todo_write 并发重写。审批侧由 sandbox 的 session-state 豁免放行
        //（不改文件系统、不 spawn 进程），见 wavecode-sandbox 的 is_session_state。
        false
    }

    async fn execute(&self, input: Value, _ctx: &ToolCtx) -> Result<ToolOutput> {
        let err = |reason: String| {
            Ok(ToolOutput {
                content: reason,
                is_error: true,
            })
        };
        let Some(todos) = input.get("todos").and_then(Value::as_array) else {
            return err("missing or invalid parameter 'todos' (array required)".to_owned());
        };
        if todos.len() > MAX_TODOS {
            return err(format!("too many todos: {} (max {MAX_TODOS})", todos.len()));
        }
        let mut items = Vec::with_capacity(todos.len());
        for (i, raw) in todos.iter().enumerate() {
            let Some(content) = raw.get("content").and_then(Value::as_str) else {
                return err(format!(
                    "todos[{i}]: missing or invalid 'content' (string required)"
                ));
            };
            if content.trim().is_empty() {
                return err(format!("todos[{i}]: 'content' must not be empty"));
            }
            let Some(status_raw) = raw.get("status").and_then(Value::as_str) else {
                return err(format!(
                    "todos[{i}]: missing or invalid 'status' (string required)"
                ));
            };
            let Some(status) = TodoStatus::parse(status_raw) else {
                return err(format!(
                    "todos[{i}]: invalid status {status_raw:?} \
                     (expected pending | in_progress | completed)"
                ));
            };
            // id 按位置自动生成（调用方不维护 id，见模块注释）。
            items.push(TodoItem {
                id: (i + 1).to_string(),
                content: content.to_owned(),
                status,
            });
        }
        self.store.write(items);
        // 回显当前清单（编号 + 状态标记），供模型确认写入结果。
        Ok(ToolOutput {
            content: format!(
                "Task list updated:\n{}",
                format_todos(&self.store.snapshot())
            ),
            is_error: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ToolCtx {
        ToolCtx {
            cwd: std::path::PathBuf::from("."),
            deny_env: Vec::new(),
        }
    }

    #[tokio::test]
    async fn write_replaces_list_and_echoes() {
        let store = TodoStore::default();
        let tool = TodoWrite::new(store.clone());
        let out = tool
            .execute(
                json!({"todos": [
                    {"content": "读代码", "status": "completed"},
                    {"content": "写实现", "status": "in_progress"},
                    {"content": "跑测试", "status": "pending"},
                ]}),
                &ctx(),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("1. [completed] 读代码"));
        assert!(out.content.contains("2. [in_progress] 写实现"));
        assert!(out.content.contains("3. [pending] 跑测试"));
        // 共享句柄读到同一份状态；id 按位置生成。
        let snap = store.snapshot();
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].id, "1");
        assert_eq!(store.unfinished(), (1, 1));

        // 整体重写：第二次调用全量替换（不保留旧条目）。
        tool.execute(
            json!({"todos": [{"content": "收尾", "status": "pending"}]}),
            &ctx(),
        )
        .await
        .unwrap();
        let snap = store.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].content, "收尾");
    }

    #[tokio::test]
    async fn empty_array_clears_list() {
        let store = TodoStore::default();
        let tool = TodoWrite::new(store.clone());
        tool.execute(
            json!({"todos": [{"content": "x", "status": "pending"}]}),
            &ctx(),
        )
        .await
        .unwrap();
        let out = tool.execute(json!({"todos": []}), &ctx()).await.unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("Task list is empty."));
        assert_eq!(store.unfinished(), (0, 0));
    }

    #[tokio::test]
    async fn invalid_input_is_error_output_not_panic() {
        let tool = TodoWrite::new(TodoStore::default());
        // 缺 todos
        assert!(tool.execute(json!({}), &ctx()).await.unwrap().is_error);
        // 非法 status
        let out = tool
            .execute(
                json!({"todos": [{"content": "x", "status": "done"}]}),
                &ctx(),
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("invalid status"));
        // 空 content
        assert!(
            tool.execute(
                json!({"todos": [{"content": "  ", "status": "pending"}]}),
                &ctx()
            )
            .await
            .unwrap()
            .is_error
        );
        // 缺 content / status
        assert!(
            tool.execute(json!({"todos": [{"status": "pending"}]}), &ctx())
                .await
                .unwrap()
                .is_error
        );
        // 超上限
        let many: Vec<Value> = (0..=MAX_TODOS)
            .map(|i| json!({"content": format!("t{i}"), "status": "pending"}))
            .collect();
        assert!(
            tool.execute(json!({"todos": many}), &ctx())
                .await
                .unwrap()
                .is_error
        );
    }
}
