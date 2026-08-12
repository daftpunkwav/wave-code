//! P10 会话持久化（SPEC §16）：rollout jsonl 追加写 + replay 恢复 + resume 列表。
//!
//! - **rollout 文件**：`<root>/<thread-id>.jsonl`（生产 root =
//!   `~/.wavecode/threads/`，测试注入根目录），每行一条带序号的持久化
//!   记录（用户输入 / assistant 消息 / 工具调用与结果 / 压缩记录），追加写。
//!   序号从 1 起单调递增，由写入方分配；同一文件的多进程并发写不在首版
//!   支持范围（单会话单进程语义）。
//! - **恢复语义**：replay rollout 重建消息历史——压缩记录承载压缩后的完整
//!   新历史（摘要 + 最近 N 条原文，与 P3 压缩管线配合），replay 遇压缩记录
//!   即以其重置历史、其后记录继续追加：压缩点之后原文 + 摘要即新历史。
//!   崩溃截断容忍：末行半行（进程死在写中途）警告后忽略；replay 产物统一
//!   经 [`wavecode_context::normalize_history`] 兜底配对完整。
//! - **SQLite 索引降级**（诚实声明）：SPEC §16 的 `threads.db` 索引（标题 /
//!   全文检索）首版降级为 [`list_threads`] 的文件 mtime 倒序 + 首条用户
//!   消息摘要；检索与标题字段留待后续迭代，不阻塞 resume 主路径。
//! - **fork 占位**：SPEC §16 的 fork（复制 rollout 到指定序号）非本阶段
//!   必须——[`replay`] 的折叠语义已具备"取前缀 replay"的基础，复制入口
//!   随 resume 交互完善落地。

use std::io::{BufRead, Write as _};
use std::path::{Path, PathBuf};

use wavecode_llm::{ContentBlock, Message, Role};
use wavecode_protocol::CompactTrigger;

/// threads 目录名（`~/.wavecode/` 下）。
pub const THREADS_DIR: &str = "threads";

/// 列表摘要的字符上限（首条用户消息单行截断）。
const SUMMARY_MAX_CHARS: usize = 60;

/// 会话持久化装配（`SessionConfig.rollout`；`None` = 不持久化——子代理
/// 自身的 Session 即此形态：隔离上下文，持久化以父会话为单位）。
#[derive(Debug, Clone)]
pub struct RolloutConfig {
    /// threads 根目录（生产 = `~/.wavecode/threads`，测试注入临时目录）。
    pub root: PathBuf,
    /// 会话 thread id（rollout 文件名 `<thread-id>.jsonl`；仅允许 ASCII
    /// 字母数字 / `-` / `_`——id 来自 CLI 参数，白名单防路径逃逸）。
    pub thread_id: String,
}

/// 持久化记录（rollout 文件每行一条，带序号；`kind` 为线型 tag）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RolloutRecord {
    /// 一条历史消息（用户输入 / assistant 消息 / 工具结果回灌消息）。
    Message {
        /// 记录序号（从 1 起单调递增）。
        seq: u64,
        /// 消息本体。
        message: Message,
    },
    /// 压缩记录：承载压缩后的完整新历史（replay 遇此记录即重置历史）。
    Compaction {
        /// 记录序号。
        seq: u64,
        /// 压缩触发来源（观测面，replay 不消费）。
        trigger: CompactTrigger,
        /// 摘要 token 估算（观测面）。
        summary_tokens: u64,
        /// 压缩后的完整新历史（摘要消息 + 最近 N 条原文）。
        messages: Vec<Message>,
    },
}

impl RolloutRecord {
    /// 记录序号。
    pub fn seq(&self) -> u64 {
        match self {
            Self::Message { seq, .. } | Self::Compaction { seq, .. } => *seq,
        }
    }
}

/// thread id 校验：非空、长度有界、仅 ASCII 字母数字 / `-` / `_`
///（id 来自 CLI 参数，白名单防 `../` 路径逃逸）。
pub fn is_valid_thread_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// 生产根目录：`~/.wavecode/threads/`。
pub fn default_root(home: &Path) -> PathBuf {
    home.join(".wavecode").join(THREADS_DIR)
}

/// rollout 文件路径（校验 thread id，非法 id 返回 InvalidInput 错误）。
pub fn rollout_path(root: &Path, thread_id: &str) -> std::io::Result<PathBuf> {
    if !is_valid_thread_id(thread_id) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("非法 thread id: {thread_id:?}（仅允许 ASCII 字母数字 / - / _）"),
        ));
    }
    Ok(root.join(format!("{thread_id}.jsonl")))
}

/// rollout 追加写记录器（持有打开的文件句柄；序号接续既有记录）。
///
/// 写失败即停用并 `tracing::warn` 留痕：持久化是辅助面，失败不阻塞会话
///（显式降级，不静默——日志可见；cli bootstrap 在装配期已预建目录并
/// 预警，运行期写失败只剩磁盘满 / 权限变化等异常形态）。
pub struct RolloutRecorder {
    file: std::fs::File,
    next_seq: u64,
    /// 写入失败后停用（warn 留痕，不反复报错刷屏）。
    disabled: bool,
}

impl RolloutRecorder {
    /// 打开（不存在则创建）rollout 文件，追加写；`next_seq` 接续既有记录。
    pub fn open(path: &Path, next_seq: u64) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        Ok(Self {
            file,
            next_seq,
            disabled: false,
        })
    }

    /// 追加一条记录（序列化 + 写 + flush）。失败停用（见结构体注释）。
    fn append(&mut self, record: RolloutRecord) {
        if self.disabled {
            return;
        }
        let seq = record.seq();
        let mut line = match serde_json::to_string(&record) {
            Ok(line) => line,
            Err(e) => {
                tracing::warn!(error = %e, "rollout 记录序列化失败，持久化停用");
                self.disabled = true;
                return;
            }
        };
        line.push('\n');
        if let Err(e) = self
            .file
            .write_all(line.as_bytes())
            .and_then(|()| self.file.flush())
        {
            tracing::warn!(error = %e, "rollout 写入失败，持久化停用");
            self.disabled = true;
            return;
        }
        debug_assert_eq!(seq, self.next_seq, "rollout 序号须连续递增");
        self.next_seq += 1;
    }

    /// 记录一条历史消息。
    pub fn record_message(&mut self, message: &Message) {
        self.append(RolloutRecord::Message {
            seq: self.next_seq,
            message: message.clone(),
        });
    }

    /// 记录一次压缩（承载压缩后的完整新历史，replay 的重置点）。
    pub fn record_compaction(
        &mut self,
        trigger: CompactTrigger,
        summary_tokens: u64,
        messages: &[Message],
    ) {
        self.append(RolloutRecord::Compaction {
            seq: self.next_seq,
            trigger,
            summary_tokens,
            messages: messages.to_vec(),
        });
    }
}

/// `load_rollout` 产物。
#[derive(Debug)]
pub struct RolloutLoad {
    /// 已解析的记录（按文件序）。
    pub records: Vec<RolloutRecord>,
    /// 下一条记录的序号（最大序号 + 1；空文件为 1）。
    pub next_seq: u64,
    /// 容忍性警告（崩溃半行等；记录本身已跳过）。
    pub warnings: Vec<String>,
}

/// 读取 rollout 文件：逐行解析；损坏行（典型：进程死在写中途的半行）
/// 警告后忽略该行及其后内容（前缀记录仍可用于恢复）。
pub fn load_rollout(path: &Path) -> std::io::Result<RolloutLoad> {
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);
    let mut records = Vec::new();
    let mut warnings = Vec::new();
    let mut next_seq = 1u64;
    for (idx, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<RolloutRecord>(&line) {
            Ok(record) => {
                next_seq = next_seq.max(record.seq() + 1);
                records.push(record);
            }
            Err(e) => {
                warnings.push(format!(
                    "rollout 第 {} 行记录损坏（可能为崩溃半行），已忽略该行及其后内容: {e}",
                    idx + 1
                ));
                break;
            }
        }
    }
    Ok(RolloutLoad {
        records,
        next_seq,
        warnings,
    })
}

/// replay：折叠记录重建消息历史——压缩记录重置历史（压缩点之后原文 +
/// 摘要即新历史，SPEC §16）；产物经 `normalize_history` 兜底配对完整
///（崩溃可能留下半截 tool_use 配对）。
pub fn replay(records: &[RolloutRecord]) -> Vec<Message> {
    let mut history: Vec<Message> = Vec::new();
    for record in records {
        match record {
            RolloutRecord::Message { message, .. } => history.push(message.clone()),
            RolloutRecord::Compaction { messages, .. } => history = messages.clone(),
        }
    }
    wavecode_context::normalize_history(&history)
}

/// 会话构造的 rollout 入口（`Session::new` 调用）：已存在的 rollout 文件
/// replay 恢复历史（resume 语义），随后打开追加写记录器（序号接续）。
/// 任一步失败都显式 warn 降级（不阻塞会话），不静默。
pub(crate) fn open_session_rollout(cfg: &RolloutConfig) -> (Vec<Message>, Option<RolloutRecorder>) {
    let path = match rollout_path(&cfg.root, &cfg.thread_id) {
        Ok(path) => path,
        Err(e) => {
            tracing::warn!(error = %e, "rollout 路径非法，会话不持久化");
            return (Vec::new(), None);
        }
    };
    let mut history = Vec::new();
    let mut next_seq = 1;
    if path.exists() {
        match load_rollout(&path) {
            Ok(load) => {
                for warning in &load.warnings {
                    tracing::warn!(warning = %warning, "rollout 恢复警告");
                }
                history = replay(&load.records);
                next_seq = load.next_seq;
                if !history.is_empty() {
                    tracing::info!(
                        messages = history.len(),
                        path = %path.display(),
                        "会话已从 rollout 恢复"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "rollout 读取失败，按新会话开始")
            }
        }
    }
    match RolloutRecorder::open(&path, next_seq) {
        Ok(recorder) => (history, Some(recorder)),
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "rollout 打开失败，会话不持久化");
            (history, None)
        }
    }
}

/// `wavecode resume` 列表条目。
#[derive(Debug)]
pub struct ThreadInfo {
    /// 会话 thread id（文件名 stem）。
    pub thread_id: String,
    /// rollout 文件 mtime（列表排序键）。
    pub modified: std::time::SystemTime,
    /// replay 后的当前历史条数。
    pub message_count: usize,
    /// 压缩次数。
    pub compaction_count: usize,
    /// 首条用户消息摘要（单行截断；无则 None）。
    pub first_user_text: Option<String>,
}

/// 列出根目录下的会话（mtime 倒序；SQLite 索引的首版降级形态，见模块
/// 注释）。根目录不存在返回空清单；单个损坏文件警告跳过，不炸列表。
pub fn list_threads(root: &Path) -> std::io::Result<Vec<ThreadInfo>> {
    let mut out = Vec::new();
    if !root.is_dir() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(thread_id) = path.file_stem().and_then(|s| s.to_str()).map(str::to_owned) else {
            continue;
        };
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        let load = match load_rollout(&path) {
            Ok(load) => load,
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "rollout 读取失败，列表跳过");
                continue;
            }
        };
        let history = replay(&load.records);
        let compaction_count = load
            .records
            .iter()
            .filter(|r| matches!(r, RolloutRecord::Compaction { .. }))
            .count();
        out.push(ThreadInfo {
            thread_id,
            modified,
            message_count: history.len(),
            compaction_count,
            first_user_text: first_user_text(&history),
        });
    }
    out.sort_by_key(|t| std::cmp::Reverse(t.modified));
    Ok(out)
}

/// 列表摘要：首条用户文本消息（跳过压缩摘要 meta 消息与工具结果），
/// 合并空白为单行并截断。
fn first_user_text(history: &[Message]) -> Option<String> {
    history
        .iter()
        .filter(|m| m.role == Role::User)
        .flat_map(|m| m.content.iter())
        .find_map(|b| match b {
            ContentBlock::Text { text }
                if !text.starts_with(wavecode_context::SUMMARY_MESSAGE_PREFIX) =>
            {
                let one_line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
                let mut out: String = one_line.chars().take(SUMMARY_MAX_CHARS).collect();
                if one_line.chars().count() > SUMMARY_MAX_CHARS {
                    out.push('…');
                }
                Some(out)
            }
            _ => None,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_text(text: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: text.to_owned(),
            }],
        }
    }

    fn assistant_tool_use(id: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: id.to_owned(),
                name: "read_file".to_owned(),
                input: serde_json::json!({"path": "a.txt"}),
            }],
        }
    }

    fn tool_result(id: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id.to_owned(),
                content: "A".to_owned(),
                is_error: false,
            }],
        }
    }

    /// 记录线型与 roundtrip（线格式漂移会破坏既有 rollout 的兼容性）。
    #[test]
    fn record_wire_format_roundtrip() {
        let msg = RolloutRecord::Message {
            seq: 1,
            message: user_text("你好"),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""kind":"message""#), "{json}");
        assert!(json.contains(r#""seq":1"#), "{json}");
        assert_eq!(serde_json::from_str::<RolloutRecord>(&json).unwrap(), msg);

        let compact = RolloutRecord::Compaction {
            seq: 2,
            trigger: CompactTrigger::Auto,
            summary_tokens: 42,
            messages: vec![user_text(
                "[上下文压缩] 早前对话已压缩为以下摘要：\n\n## 目标\nT",
            )],
        };
        let json = serde_json::to_string(&compact).unwrap();
        assert!(json.contains(r#""kind":"compaction""#), "{json}");
        assert!(json.contains(r#""trigger":"auto""#), "{json}");
        assert_eq!(
            serde_json::from_str::<RolloutRecord>(&json).unwrap(),
            compact
        );
    }

    /// replay 折叠语义：压缩记录重置历史，其后记录继续追加。
    #[test]
    fn replay_resets_history_at_compaction_record() {
        let records = vec![
            RolloutRecord::Message {
                seq: 1,
                message: user_text("第一条"),
            },
            RolloutRecord::Message {
                seq: 2,
                message: user_text("第二条"),
            },
            RolloutRecord::Compaction {
                seq: 3,
                trigger: CompactTrigger::Auto,
                summary_tokens: 10,
                messages: vec![user_text("摘要+保留尾")],
            },
            RolloutRecord::Message {
                seq: 4,
                message: user_text("压缩后的新消息"),
            },
        ];
        let history = replay(&records);
        assert_eq!(history.len(), 2, "压缩记录重置历史: {history:?}");
        assert!(
            matches!(&history[0].content[0], ContentBlock::Text { text } if text == "摘要+保留尾")
        );
        assert!(
            matches!(&history[1].content[0], ContentBlock::Text { text } if text == "压缩后的新消息")
        );
    }

    /// replay 兜底配对：崩溃留下半截 tool_use（无配对结果）时
    /// normalize_history 补 is_error 结果，恢复后配对完整。
    #[test]
    fn replay_normalizes_dangling_tool_use_from_crash() {
        let records = vec![
            RolloutRecord::Message {
                seq: 1,
                message: user_text("干活"),
            },
            RolloutRecord::Message {
                seq: 2,
                message: assistant_tool_use("t1"),
            },
            // 崩溃：tool_result 未来得及落盘
        ];
        let history = replay(&records);
        assert_eq!(
            wavecode_context::find_pairing_violations(&history),
            Vec::<String>::new()
        );
        let last = history.last().unwrap();
        assert!(last.content.iter().any(
            |b| matches!(b, ContentBlock::ToolResult { tool_use_id, is_error: true, .. } if tool_use_id == "t1")
        ));
        // 配对完整时原样通过（与 normalize 幂等测试同例）。
        let paired = vec![
            RolloutRecord::Message {
                seq: 1,
                message: assistant_tool_use("t1"),
            },
            RolloutRecord::Message {
                seq: 2,
                message: tool_result("t1"),
            },
        ];
        let history = replay(&paired);
        assert_eq!(history.len(), 2);
        assert_eq!(
            wavecode_context::find_pairing_violations(&history),
            Vec::<String>::new()
        );
    }

    /// 追加写 → 读取：序号从 1 连续递增；再次打开序号接续（resume 续写）。
    #[test]
    fn recorder_appends_and_next_seq_continues() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("threads/t-1.jsonl");
        let mut rec = RolloutRecorder::open(&path, 1).unwrap();
        rec.record_message(&user_text("一"));
        rec.record_message(&user_text("二"));
        rec.record_compaction(CompactTrigger::Manual, 7, &[user_text("摘要")]);
        drop(rec);

        let load = load_rollout(&path).unwrap();
        assert!(load.warnings.is_empty());
        assert_eq!(load.records.len(), 3);
        let seqs: Vec<u64> = load.records.iter().map(|r| r.seq()).collect();
        assert_eq!(seqs, vec![1, 2, 3]);
        assert_eq!(load.next_seq, 4);

        // 模拟 resume：以 next_seq 接续打开，序号不重号。
        let mut rec = RolloutRecorder::open(&path, load.next_seq).unwrap();
        rec.record_message(&user_text("三"));
        drop(rec);
        let load = load_rollout(&path).unwrap();
        let seqs: Vec<u64> = load.records.iter().map(|r| r.seq()).collect();
        assert_eq!(seqs, vec![1, 2, 3, 4]);
    }

    /// 崩溃半行容忍：末行截断时前缀记录仍可恢复，警告可见。
    #[test]
    fn load_tolerates_truncated_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t-2.jsonl");
        let good = serde_json::to_string(&RolloutRecord::Message {
            seq: 1,
            message: user_text("完整记录"),
        })
        .unwrap();
        // 半行：进程死在写中途
        std::fs::write(
            &path,
            format!("{good}\n{{\"kind\":\"message\",\"seq\":2,\"me"),
        )
        .unwrap();
        let load = load_rollout(&path).unwrap();
        assert_eq!(load.records.len(), 1);
        assert_eq!(load.warnings.len(), 1);
        assert!(load.warnings[0].contains("第 2 行"));
        assert_eq!(replay(&load.records), vec![user_text("完整记录")]);
    }

    /// thread id 白名单：路径逃逸 / 分隔符 / 空串一律拒绝。
    #[test]
    fn invalid_thread_id_rejected() {
        assert!(is_valid_thread_id("abc-123_DEF"));
        for bad in ["", "../etc", "a/b", "a\\b", "a.b", "中"] {
            assert!(!is_valid_thread_id(bad), "{bad:?} 应拒绝");
            assert!(rollout_path(Path::new("/tmp"), bad).is_err());
        }
    }

    /// 列表：mtime 倒序 + 首条用户消息摘要 + 压缩计数；根目录不存在
    /// 返回空清单；损坏文件跳过。
    #[test]
    fn list_threads_orders_by_mtime_with_summary() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("threads");
        std::fs::create_dir_all(&root).unwrap();
        let write = |id: &str, text: &str| {
            let mut rec = RolloutRecorder::open(&root.join(format!("{id}.jsonl")), 1).unwrap();
            rec.record_message(&user_text(text));
            let summary = format!("{}\n\n摘要", wavecode_context::SUMMARY_MESSAGE_PREFIX);
            rec.record_compaction(CompactTrigger::Auto, 1, &[user_text(&summary)]);
            rec.record_message(&user_text("压缩后"));
        };
        write("older", "旧会话的第一句话\n带换行");
        // mtime 排序依赖写入时刻可区分：NTFS/ext4 的 mtime 粒度均远细于
        // 50ms，睡一拍消除粒度巧合（不引入平台 mtime 设定 API）。
        std::thread::sleep(std::time::Duration::from_millis(50));
        write("newer", "新会话目标：搭建电商平台");

        let threads = list_threads(&root).unwrap();
        assert_eq!(threads.len(), 2);
        assert_eq!(threads[0].thread_id, "newer", "mtime 倒序");
        assert_eq!(threads[1].thread_id, "older");
        assert_eq!(threads[0].compaction_count, 1);
        // replay 后历史 = 摘要 + 压缩后 = 2 条。
        assert_eq!(threads[0].message_count, 2);
        // 首条用户文本取 replay 后可见历史：压缩重置了压缩前消息，
        // 跳过后带前缀的摘要 meta 消息，落到压缩后的用户文本。
        assert_eq!(threads[0].first_user_text.as_deref(), Some("压缩后"));
        // 换行合并为单行；跳过压缩摘要 meta 消息取压缩后的用户文本。
        assert_eq!(threads[1].first_user_text.as_deref(), Some("压缩后"));

        // 根目录不存在 → 空清单。
        assert!(list_threads(&dir.path().join("nope")).unwrap().is_empty());
        // 非 jsonl 文件忽略。
        std::fs::write(root.join("notes.txt"), "x").unwrap();
        assert_eq!(list_threads(&root).unwrap().len(), 2);
    }

    /// 无用户文本的会话：摘要为 None（列表显示侧有占位文案）。
    #[test]
    fn list_threads_without_user_text_gives_none() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("threads");
        let mut rec = RolloutRecorder::open(&root.join("empty.jsonl"), 1).unwrap();
        rec.record_message(&Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: "x".into() }],
        });
        drop(rec);
        let threads = list_threads(&root).unwrap();
        assert_eq!(threads[0].first_user_text, None);
    }
}
