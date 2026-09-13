//! wavecode-memory — memory system (P6, SPEC section 7).
//!
//! - Instruction memory ([`instructions`]): layered `WAVECODE.md` discovery
//!   (user level -> project root -> cwd) with `@path` recursive references
//!   (depth cap 5, cycle-safe) merged with `.wavecode/rules/*.md` rule dirs;
//! - Persistent memory ([`store`]): user / feedback / project / reference
//!   entry files plus a `MEMORY.md` index; the root is injectable (production
//!   uses `~/.wavecode/memories/`, tests use tempfile);
//! - Auto-extract output parsing ([`extract`]): parses a subagent's
//!   line-format output at session end back into a (category, content) list;
//! - Heuristic consolidation ([`consolidate`]): a "dream"-style background
//!   merge run at session end — near-duplicate entries (Jaccard >= 0.6 on
//!   lowercased word sets) within a category fold into their newest wording.
//!
//! This crate has no workspace-internal dependencies (SPEC section 3 matrix):
//! the `memory_write` tool, approval wiring, prompt injection, and extraction
//! orchestration all live on the core side (the core->memory edge is allowed).
//!
//! Consolidation is heuristic v1 (see [`consolidate`] for the exact limits).
//! Still unimplemented from SPEC section 7.2: the 24h + 5-session gate
//! (consolidation currently runs after every extraction), stale-entry pruning,
//! and LLM/embedding-based similarity; `WAVECODE.override.md` overrides and
//! fallback filenames (CLAUDE.md/AGENTS.md) are likewise later work.

pub mod consolidate;
pub mod extract;
pub mod instructions;
pub mod store;

pub use consolidate::{MERGE_SIMILARITY, plan_merges};
pub use extract::parse_extracted_entries;
pub use instructions::{InstructionMemory, MAX_INCLUDE_DEPTH, collect, find_project_root};
pub use store::{INDEX_FILE, MemoryCategory, MemoryStore};
