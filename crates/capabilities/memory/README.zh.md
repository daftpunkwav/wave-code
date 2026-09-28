# crates/capabilities/memory/ — 指令记忆与持久记忆

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `src/lib.rs` | Crate 地图与再导出（`collect`、`find_project_root`、`MemoryStore`、`plan_merges`、`parse_extracted_entries` 等） |
| `src/instructions.rs` | 指令记忆：分层 `AGENTS.md` 发现（用户级 -> 项目根 -> cwd；项目根沿 `.git` 向上定位）加递归 `@path` 引用展开（深度上限 5、环安全），每层 `AGENTS.local.md` 补充（无回退文件名：没有 `AGENTS.md` 则整层跳过），并与 `.wavecode/rules/*.md` 规则目录合并 |
| `src/store.rs` | `MemoryStore`：一个 `MEMORY.md` 索引加四个分类条目文件（`user` / `feedback` / `project` / `reference`），Markdown 列表项形态；只追加，根目录可注入（生产为 `~/.wavecode/memories/`，测试用 tempfile） |
| `src/extract.rs` | `parse_extracted_entries`：把子代理输出的 `[category] content` 行格式解析回 `(category, content)` 对；未知标签、空条目、标签前的闲话一律丢弃——模型输出不可信 |
| `src/consolidate.rs` | 启发式合并：纯函数 `plan_merges` 规划器把近重复条目（小写词集 Jaccard >= 0.6，限同一分类内）折叠进最新措辞；所有 IO 都在薄薄的 `MemoryStore::consolidate` 包装里 |
| `src/tool.rs` | `MemoryWrite`——`memory_write` 工具（共享 `Tool` trait），用于显式的持久写入；动文件系统前先校验 category 与 content |

存储是普通、用户可直接编辑的 Markdown，本 crate 对它只做追加。除
`wavecode-tools` 这条边（`tool.rs` 实现共享 `Tool` trait——与 `wavecode-mcp`
同层的取用方式）外，crate 没有其他工作区内部依赖：`memory_write` 的审批
接线、prompt 注入与抽取编排都在 core 侧。写入是小体量追加，由调用方用
`spawn_blocking` 挪出 executor；读取在启动装配时一次完成。
