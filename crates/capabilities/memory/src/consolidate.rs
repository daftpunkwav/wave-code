/*!
 * @file MemoryConsolidation
 * @description Heuristic near-duplicate merge over memory store entries.
 *
 * Responsibilities:
 * - Plan merges among near-duplicate entries (pure Jaccard-overlap
 *   planner, deterministic, no model calls).
 * - Rewrite affected category files and rebuild the index only when
 *   something was dropped.
 *
 * This module must not depend on: models, embeddings, or any capability
 * outside the memory crate's own store.
 */

//! Heuristic memory consolidation (SPEC section 7.2, first version): a
//! "dream"-style background merge run at session end, after auto-extraction
//! appended new entries. Near-duplicate entries within one category fold into
//! their newest occurrence, so re-extraction of the same fact across sessions
//! does not accumulate bullets forever.
//!
//! The planner is a pure function of the entry texts — no IO, no model calls,
//! no embeddings — so it is deterministic and unit-testable; all IO lives in
//! the thin [`MemoryStore::consolidate`] wrapper below.
//!
//! Similarity is Jaccard over lowercased alphanumeric word sets. Deliberate
//! limits (heuristic v1, honest disclosure):
//! - merges stay within one category (entries live in per-category files;
//!   cross-category near-duplicates are left alone);
//! - the kept entry keeps the newest content verbatim — the store has no
//!   tags, so there is nothing to union into the survivor;
//! - the SPEC's 24h + 5-session gate is not implemented: consolidation runs
//!   after every extraction (cheap: short entries, O(n^2) word-set compares).

use std::collections::HashSet;

use crate::store::{INDEX_FILE, MemoryCategory, MemoryStore, format_entry, summarize};

/// Jaccard similarity at or above which two entries count as near-duplicates.
pub const MERGE_SIMILARITY: f64 = 0.6;

/// Plan merges over one category's entries (file order: oldest first).
///
/// Returns the ascending indices of entries to drop: whenever an older entry
/// is a near-duplicate of a newer one (Jaccard >= `similarity` on lowercased
/// word sets), the older index is dropped and the newer keeps its content
/// verbatim. The plan is deterministic — pairs are scanned by ascending
/// newer index and an already-absorbed entry is never re-compared — and
/// pure: no IO, no randomness. Entries with empty word sets (blank or
/// punctuation-only) never match anything (Jaccard is defined as 0.0 there).
pub fn plan_merges(entries: &[String], similarity: f64) -> Vec<usize> {
    let sets: Vec<HashSet<String>> = entries.iter().map(|e| word_set(e)).collect();
    let mut absorbed = vec![false; entries.len()];
    let mut plan = Vec::new();
    for (newer, newer_set) in sets.iter().enumerate() {
        for (older, older_set) in sets.iter().take(newer).enumerate() {
            if absorbed[older] {
                continue; // already merged into an even newer entry
            }
            if jaccard(older_set, newer_set) >= similarity {
                absorbed[older] = true;
                plan.push(older);
            }
        }
    }
    plan.sort_unstable();
    plan
}

impl MemoryStore {
    /// Fold near-duplicate entries (best-effort background merge, see the
    /// module docs): per category, drop the entries planned by [`plan_merges`]
    /// (older ones absorbed into the newest near-duplicate) and rewrite the
    /// affected category file; when anything was dropped, the index is rebuilt
    /// from the surviving entries so every `- [category] summary` row stays
    /// 1:1 with an entry (rows regroup by category instead of arrival order —
    /// the index is for navigation, nothing links into it by line number).
    /// Returns the number of entries dropped. An empty store is `Ok(0)` and
    /// touches nothing: no files are written and no directories are created.
    pub fn consolidate(&self) -> std::io::Result<usize> {
        let mut dropped = 0;
        let mut changed = false;
        for category in MemoryCategory::ALL {
            let entries = parse_category_entries(&self.read_category(category)?);
            if entries.is_empty() {
                continue;
            }
            let plan = plan_merges(&entries, MERGE_SIMILARITY);
            if plan.is_empty() {
                continue;
            }
            let mut absorbed = vec![false; entries.len()];
            for i in plan {
                absorbed[i] = true;
                dropped += 1;
            }
            let mut body = String::new();
            for (i, entry) in entries.iter().enumerate() {
                if !absorbed[i] {
                    body.push_str(&format_entry(entry));
                    body.push('\n');
                }
            }
            std::fs::write(self.root().join(category.file_name()), body)?;
            changed = true;
        }
        if changed {
            let mut index = String::new();
            for category in MemoryCategory::ALL {
                for entry in parse_category_entries(&self.read_category(category)?) {
                    index.push_str(&format!(
                        "- [{}] {} (see {})\n",
                        category.as_str(),
                        summarize(&entry),
                        category.file_name()
                    ));
                }
            }
            std::fs::write(self.root().join(INDEX_FILE), index)?;
        }
        Ok(dropped)
    }
}

/// Parse one category file back into entry contents — the inverse of the
/// store's append format: a `- ` bullet starts an entry and lines indented
/// two spaces continue it (multi-line content is stored with `\n` replaced
/// by `\n  `, so stripping exactly two spaces reconstructs the content).
/// Anything else (blank lines, a continuation with no open bullet) is
/// ignored; by construction the files only contain what `append` wrote.
fn parse_category_entries(text: &str) -> Vec<String> {
    let mut entries: Vec<String> = Vec::new();
    for line in text.lines() {
        if let Some(first) = line.strip_prefix("- ") {
            entries.push(first.to_owned());
        } else if let Some(rest) = line.strip_prefix("  ")
            && let Some(last) = entries.last_mut()
        {
            last.push('\n');
            last.push_str(rest);
        }
    }
    entries
}

/// Jaccard similarity of two word sets: |a ∩ b| / |a ∪ b|. Two empty sets
/// (nothing comparable) count as 0.0, never a match.
fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count();
    let union = a.len() + b.len() - inter;
    inter as f64 / union as f64
}

/// Lowercased alphanumeric word set: split on every non-alphanumeric char so
/// punctuation and casing never mask a duplicate ("Repo uses pnpm." equals
/// "repo uses pnpm").
fn word_set(text: &str) -> HashSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// Merge case: the older near-duplicate (exactly at the 0.6 threshold,
    /// 3/5 shared words) is absorbed into the newest wording; an unrelated
    /// entry survives untouched.
    #[test]
    fn older_near_duplicate_is_dropped_newest_kept() {
        let entries = s(&[
            "repo uses pnpm workspaces",
            "repo uses pnpm workspaces consistently", // 4/5 = 0.8
            "ci runs on linux",                       // unrelated
            "alpha beta gamma",
            "alpha beta gamma delta epsilon", // 3/5 = 0.6, exactly at the gate
        ]);
        assert_eq!(plan_merges(&entries, MERGE_SIMILARITY), vec![0, 3]);
    }

    /// No-merge case: dissimilar entries — and a same-topic paraphrase below
    /// the threshold (2/4 = 0.5) — all survive.
    #[test]
    fn dissimilar_entries_plan_no_merges() {
        let entries = s(&[
            "repo uses pnpm",
            "prefers compact replies",
            "repo uses yarn",
        ]);
        assert!(plan_merges(&entries, MERGE_SIMILARITY).is_empty());
    }

    /// Tie/order case: three mutually identical entries coalesce into the
    /// newest, dropping both older indices in ascending order.
    #[test]
    fn duplicate_chain_collapses_to_newest() {
        let entries = s(&["alpha beta", "alpha beta", "alpha beta"]);
        assert_eq!(plan_merges(&entries, MERGE_SIMILARITY), vec![0, 1]);
    }

    /// Tie/order case: the plan is deterministic and sorted even when drops
    /// are discovered out of index order — here the oldest entry survives the
    /// middle pair (0.5 < 0.6) but is finally absorbed by the newest, so it
    /// is planned after the middle entry it outranks.
    #[test]
    fn plan_is_deterministic_and_sorted() {
        let entries = s(&["alpha", "alpha beta", "alpha beta", "alpha"]);
        assert_eq!(plan_merges(&entries, MERGE_SIMILARITY), vec![0, 1]);
    }

    /// Store-level merge: the older near-duplicate (6/7 = 0.86) is dropped,
    /// the newest — a multi-line entry — survives the rewrite with its
    /// continuation-line shape intact, and the index is rebuilt 1:1 with the
    /// surviving entries.
    #[test]
    fn store_consolidate_drops_older_duplicate_and_rewrites() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(dir.path().join("memories"));
        store
            .append(
                MemoryCategory::Feedback,
                "never refactor working code unless asked",
            )
            .unwrap();
        store
            .append(
                MemoryCategory::Feedback,
                "never refactor working code\nunless asked to",
            )
            .unwrap();

        assert_eq!(store.consolidate().unwrap(), 1);
        assert_eq!(
            store.read_category(MemoryCategory::Feedback).unwrap(),
            "- never refactor working code\n  unless asked to\n"
        );
        assert_eq!(
            store.read_index().unwrap(),
            "- [feedback] never refactor working code (see feedback.md)\n"
        );
        // Idempotent: a second pass finds nothing left to merge.
        assert_eq!(store.consolidate().unwrap(), 0);
    }

    /// An unrelated entry in another category is never compared or rewritten;
    /// the rebuilt index keeps its row.
    #[test]
    fn consolidation_stays_within_a_category() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(dir.path().join("memories"));
        store
            .append(MemoryCategory::User, "prefers compact replies")
            .unwrap();
        store
            .append(MemoryCategory::Project, "repo uses pnpm")
            .unwrap();
        store
            .append(MemoryCategory::User, "prefers compact replies please")
            .unwrap(); // 3/4 = 0.75

        assert_eq!(store.consolidate().unwrap(), 1);
        assert_eq!(
            store.read_category(MemoryCategory::User).unwrap(),
            "- prefers compact replies please\n"
        );
        assert_eq!(
            store.read_category(MemoryCategory::Project).unwrap(),
            "- repo uses pnpm\n"
        );
        let index = store.read_index().unwrap();
        let lines: Vec<&str> = index.lines().collect();
        assert_eq!(
            lines,
            [
                "- [user] prefers compact replies please (see user.md)",
                "- [project] repo uses pnpm (see project.md)",
            ]
        );
    }

    /// Empty store: consolidation is a no-op that creates no files or dirs.
    #[test]
    fn consolidate_on_empty_store_is_inert() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("memories");
        let store = MemoryStore::new(root.clone());
        assert_eq!(store.consolidate().unwrap(), 0);
        assert!(
            !root.exists(),
            "consolidation must not create the store dir"
        );
    }
}
