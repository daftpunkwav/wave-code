# crates/action/retrieval/ — lexical term-overlap retrieval over chunked documents

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `Cargo.toml` | Crate manifest; no dependencies |
| `src/lib.rs` | `chunk_text` (overlapping character windows), `retrieve` / `retrieve_with` (distinct-query-term scoring over chunks, top-k in ranked order), and the `Document` / `ScoredChunk` vocabulary |

Retrieval is pure functions over caller-owned documents, with ranking
deliberately lexical: semantic embeddings would arrive behind the same
function shape. The default geometry (400-character windows, 50 overlap)
is a default, not a constant — callers tune window and overlap per
corpus through `retrieve_with`, and empty queries match nothing.
