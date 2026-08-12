//! 系统提示词分层组装（SPEC §5.4，P4 落地）。
//!
//! 组装顺序与缓存边界（SPEC §5.4 为 prompt cache 设计）：
//!
//! ```text
//! [静态层] [动态层（cwd/git/平台）] [WAVECODE.md] [skills] [记忆索引]
//! ——缓存边界—— [会话历史] [工具结果] [用户输入]
//! ```
//!
//! - **静态层**是 const 字符串，字节级稳定——这是 prompt cache 命中率的
//!   前提，任何改动都会使既有缓存前缀失效，修改须谨慎；
//! - **动态层**集中到一处（cwd / git 分支 / 平台），易变信息不散落，
//!   避免污染静态前缀；
//! - WAVECODE.md 拼接与持久记忆索引为 P6 槽位（已落地，由装配层注入、
//!   会话内恒定）；skills 清单为 P7 槽位（已落地，SPEC §8.2：1% 窗口
//!   预算渲染、system-reminder 注入、位置在 WAVECODE.md 之后记忆索引之前）；
//! - 任务清单（todo）非空时以 `<system-reminder>` 追加在 system **尾部**
//!   （缓存边界前最末位，见 [`build_system_prompt`] 注释的取舍说明）。

use std::path::Path;

use wavecode_tools::TodoItem;

/// 静态层：角色与规则（含规划引导：复杂任务先用 todo_write 分解；长任务中
/// 定期更新清单状态）。字节稳定是 prompt cache 纪律（SPEC §5.4）。
pub const STATIC_LAYER: &str = "\
You are WaveCode, an AI coding agent operating in a CLI.
Rules:
- Use tools to act on the filesystem; prefer edit_file for modifying existing files.
- For complex or multi-step tasks, first use the todo_write tool to break the work \
into a task list; during long tasks, keep the list updated (in_progress / completed) \
as you make progress.
- Keep answers concise; after finishing tool work, summarize what you did.
- Never fabricate file contents or command results.";

/// 组装系统提示词（每轮采样前调用；清单不变时整串字节稳定）。
///
/// `instruction_memory` / `memory_index` 为 P6 记忆槽位内容（装配层在会话
/// 启动时收集，会话内恒定——其内容跨会话变化是用户行为，重建会话即得
/// 新前缀，静态层字节稳定不受影响）；`skills_catalog` 为 P7 skills 清单
/// 槽位（启动时按 1% 窗口预算渲染，会话内恒定）；`todos` 为当前任务清单
/// 快照，非空时以 `<system-reminder>` 追加在尾部。
///
/// 注入位置取舍（保持前缀稳定）：选择注入 system 尾部而非最新 user 消息侧——
/// user 侧要么对当轮历史快照做 O(n) 深拷贝（§17.5 M4 刚消除的开销），要么把
/// 提醒写进历史造成污染且每轮重复。system 尾部注入的代价是清单变化时缓存
/// 前缀失效，但清单只在 todo_write 时变化，未变化轮次整串字节稳定，cache
/// 代价有界；同时静态层与工具清单始终字节稳定。
pub async fn build_system_prompt(
    cwd: &Path,
    instruction_memory: &str,
    skills_catalog: &str,
    memory_index: &str,
    todos: &[TodoItem],
) -> String {
    let mut s = String::from(STATIC_LAYER);
    // —— 动态层：cwd / git 分支 / 平台，集中到一处（SPEC §5.4）——
    s.push_str("\n\n# Environment\nWorking directory: ");
    s.push_str(&cwd.display().to_string());
    s.push_str("\nGit branch: ");
    let branch = git_branch(cwd).await;
    s.push_str(branch.as_deref().unwrap_or("(not a git repository)"));
    s.push_str("\nPlatform: ");
    s.push_str(std::env::consts::OS);
    // —— WAVECODE.md 拼接（P6，SPEC §7.1）：装配层收集，会话内恒定 ——
    if !instruction_memory.is_empty() {
        s.push_str("\n\n# Instruction Memory (WAVECODE.md)\n");
        s.push_str(instruction_memory);
    }
    // —— skills 清单（P7，SPEC §8.2）：system-reminder 注入 name +
    //    description + when_to_use；装配层按 1% 窗口预算渲染，会话内恒定 ——
    if !skills_catalog.is_empty() {
        s.push_str(
            "\n\n<system-reminder>\nAvailable skills (invoke with the `skill` tool, or the user \
             may run `/<name> [args]`):\n",
        );
        s.push_str(skills_catalog);
        s.push_str("\n</system-reminder>");
    }
    // —— 持久记忆索引（P6，SPEC §7.2）：启动时快照。首版诚实简化：索引即
    //    全部注入，条目正文不内联——由模型按需 read_file 类别文件加载。——
    if !memory_index.is_empty() {
        s.push_str("\n\n# Persistent Memory Index\n");
        s.push_str(memory_index.trim_end());
        s.push_str(
            "\n(Entry bodies live in the memory directory's category files; load on demand \
             with read_file. Use memory_write to save new durable memories.)",
        );
    }
    // —— 缓存边界前最末位：任务清单提醒（最易变，放尾部缩小失效面）——
    if !todos.is_empty() {
        s.push_str("\n\n<system-reminder>\nCurrent task list:\n");
        s.push_str(&wavecode_tools::format_todos(todos));
        s.push_str("\n</system-reminder>");
    }
    s
}

/// 当前 git 分支（best-effort）：从 cwd 向上找 `.git/HEAD` 解析 ref；
/// 非仓库 / worktree（`.git` 为文件）/ 读取失败 → None。直接读 HEAD 文件
/// 而非 spawn git——本函数每轮组装都调用，进程开销不可接受。
async fn git_branch(cwd: &Path) -> Option<String> {
    for dir in cwd.ancestors() {
        match tokio::fs::read_to_string(dir.join(".git").join("HEAD")).await {
            Ok(content) => return parse_head(&content),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return None,
        }
    }
    None
}

/// 解析 `.git/HEAD`：`ref: refs/heads/<branch>` → 分支名；分离头指针取
/// 短 sha 标注 (detached)。
fn parse_head(content: &str) -> Option<String> {
    let c = content.trim();
    if let Some(branch) = c.strip_prefix("ref: refs/heads/") {
        return Some(branch.to_owned());
    }
    let sha = c
        .get(..8)
        .filter(|s| s.chars().all(|ch| ch.is_ascii_hexdigit()))?;
    Some(format!("{sha} (detached)"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wavecode_tools::TodoStatus;

    fn item(content: &str, status: TodoStatus) -> TodoItem {
        TodoItem {
            id: "1".into(),
            content: content.into(),
            status,
        }
    }

    /// 前缀稳定性（P4 验收）：输入不变时两次组装字节相等；静态层恒为前缀。
    #[tokio::test]
    async fn static_prefix_byte_stable() {
        let dir = tempfile::tempdir().unwrap();
        let todos = vec![item("写实现", TodoStatus::InProgress)];
        let a = build_system_prompt(dir.path(), "", "", "", &todos).await;
        let b = build_system_prompt(dir.path(), "", "", "", &todos).await;
        assert_eq!(a, b, "输入不变时两次组装必须字节相等");
        assert!(a.starts_with(STATIC_LAYER), "静态层必须是前缀");
        // 清单变化只影响尾部：静态层 + 动态层前缀仍字节相等。
        let c = build_system_prompt(dir.path(), "", "", "", &[]).await;
        assert!(a.len() > c.len());
        assert!(
            a.starts_with(&c),
            "清单变化不得污染静态层/动态层前缀:\n--- a ---\n{a}\n--- c ---\n{c}"
        );
    }

    /// 清单非空注入 system-reminder；空清单不注入。
    #[tokio::test]
    async fn todo_reminder_injected_only_when_non_empty() {
        let dir = tempfile::tempdir().unwrap();
        let with = build_system_prompt(
            dir.path(),
            "",
            "",
            "",
            &[item("跑测试", TodoStatus::Pending)],
        )
        .await;
        assert!(with.contains("<system-reminder>"));
        assert!(with.contains("1. [pending] 跑测试"));
        let without = build_system_prompt(dir.path(), "", "", "", &[]).await;
        assert!(!without.contains("<system-reminder>"));
    }

    /// 动态层内容：cwd / 平台 / 非仓库的分支占位。
    #[tokio::test]
    async fn dynamic_layer_contents() {
        let dir = tempfile::tempdir().unwrap();
        let s = build_system_prompt(dir.path(), "", "", "", &[]).await;
        assert!(s.contains(&format!("Working directory: {}", dir.path().display())));
        assert!(s.contains(&format!("Platform: {}", std::env::consts::OS)));
        assert!(s.contains("Git branch: (not a git repository)"));
    }

    /// P6/P7 记忆与 skills 槽位：WAVECODE.md → skills 清单 → 持久记忆索引
    /// 按 SPEC §5.4 顺序注入（动态层之后、任务清单之前）；空槽位不产生任何字节。
    #[tokio::test]
    async fn memory_slots_injected_in_spec_order() {
        let dir = tempfile::tempdir().unwrap();
        let s = build_system_prompt(
            dir.path(),
            "## 项目约定\n用 pnpm",
            "- commit: 创建提交 (when: 用户要求提交时)",
            "- [user] 偏好紧凑回复（详见 user.md）",
            &[item("收尾", TodoStatus::Pending)],
        )
        .await;
        let env_pos = s.find("# Environment").unwrap();
        let instr_pos = s.find("# Instruction Memory (WAVECODE.md)").unwrap();
        let skills_pos = s.find("Available skills").unwrap();
        let index_pos = s.find("# Persistent Memory Index").unwrap();
        let reminder_pos = s.rfind("<system-reminder>").unwrap();
        assert!(
            env_pos < instr_pos && instr_pos < skills_pos && skills_pos < index_pos,
            "槽位顺序应为 动态层→WAVECODE.md→skills→记忆索引:\n{s}"
        );
        assert!(index_pos < reminder_pos, "记忆索引在任务清单之前:\n{s}");
        assert!(s.contains("用 pnpm"));
        assert!(s.contains("- commit: 创建提交 (when: 用户要求提交时)"));
        assert!(s.contains("- [user] 偏好紧凑回复（详见 user.md）"));
        // 空槽位：不出现槽位标题，且不破坏前缀稳定性。
        let empty = build_system_prompt(dir.path(), "", "", "", &[]).await;
        assert!(!empty.contains("Instruction Memory"));
        assert!(!empty.contains("Available skills"));
        assert!(!empty.contains("Persistent Memory Index"));
    }

    /// git 分支解析：`.git/HEAD` 的 ref 形态与分离头指针形态。
    #[tokio::test]
    async fn git_branch_detection() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/HEAD"), "ref: refs/heads/feat-x\n").unwrap();
        assert_eq!(git_branch(dir.path()).await.as_deref(), Some("feat-x"));
        // 子目录向上查找。
        let sub = dir.path().join("a/b");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(git_branch(&sub).await.as_deref(), Some("feat-x"));
        // 分离头指针。
        std::fs::write(dir.path().join(".git/HEAD"), "0123456789abcdef\n").unwrap();
        assert_eq!(
            git_branch(dir.path()).await.as_deref(),
            Some("01234567 (detached)")
        );
        // 非仓库 → None。
        let plain = tempfile::tempdir().unwrap();
        // 注意：tempdir 在系统临时目录下，若其祖先恰为 git 仓库会误判——
        // 断言前显式确认（CI/本地 tmp 均非仓库路径）。
        if plain
            .path()
            .ancestors()
            .skip(1)
            .all(|p| !p.join(".git").exists())
        {
            assert_eq!(git_branch(plain.path()).await, None);
        }
    }
}
