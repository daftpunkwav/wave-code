//! JSONL 事件序列化（阶段 4 拆分自 render.rs）：`--json` 路径的线格式。

use super::*;

pub fn render_jsonl(ev: &Event) -> String {
    // Event 全字段可序列化，to_string 不会失败。
    serde_json::to_string(ev).expect("Event 序列化不会失败")
}
