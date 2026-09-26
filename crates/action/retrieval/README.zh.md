# crates/action/retrieval/ — 分块文档上的词项重叠检索

[English](README.md) | 中文

| 条目 | 职责 |
|---|---|
| `Cargo.toml` | crate 清单；没有任何依赖 |
| `src/lib.rs` | `chunk_text`（重叠字符窗口）、`retrieve` / `retrieve_with`（按 chunk 命中的去重查询词项数评分，top-k 排序返回），以及 `Document` / `ScoredChunk` 词汇 |

检索是作用于调用方自有文档的纯函数，排序有意采用词法匹配：语义
embedding 将来也走同样的函数形态接入。默认几何参数（400 字符窗口、
50 字符重叠）只是默认值而非常量——调用方经 `retrieve_with` 按语料
（代码、散文、日志）调整窗口与重叠；空查询不匹配任何内容。
