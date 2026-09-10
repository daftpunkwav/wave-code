/*!
 * @file PromptBuilder
 * @description System prompt assembly from named content slots.
 *
 * Responsibilities:
 * - Join instruction, memory, skill, tool, and summary slots in order.
 * - Skip empty slots so providers never see blank sections.
 * - Truncate the skill catalog to a character budget on UTF-8 boundaries.
 *
 * This module must not depend on: any other workspace crate. Callers own
 * collecting slot content (memory, skills, tools); this crate only lays
 * the prompt out.
 */

//! Pure prompt layout: deterministic section order, no orchestration.
//!
//! Dynamic per-turn assembly (budget-aware catalog sizing, summary prefix
//! rotation) composes these primitives; the policy of what fills each
//! slot stays with the driver.

/// Maximum characters kept from the skill catalog by default.
pub const DEFAULT_CATALOG_BUDGET: usize = 4_000;

/// Named content slots assembled into one system prompt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptSlots {
    /// Agent identity and operating rules; almost always present.
    pub identity: String,
    /// Project instructions (WAVECODE.md layers, rules files).
    pub instructions: String,
    /// Persistent memory index snapshot.
    pub memory_index: String,
    /// Skill catalog, pre-truncated by the caller to its budget.
    pub skill_catalog: String,
    /// Tool availability note (names the tools the model may call).
    pub tool_note: String,
    /// Compaction summary carried from the previous window.
    pub summary: String,
}

impl PromptSlots {
    /// Create empty slots for piecemeal filling.
    pub fn new() -> Self {
        Self::default()
    }
}

/// Truncate text to a character budget without splitting UTF-8.
///
/// Returns the original string when it already fits.
pub fn truncate_to_budget(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let kept: String = text.chars().take(max_chars).collect();
    format!("{kept}...[truncated]")
}

/// Assemble the system prompt from slots in a fixed section order.
///
/// Empty slots are skipped entirely: no headers, no blank lines. Section
/// order is contractual (identity first, summary last) so snapshots and
/// golden tests stay stable across refactors.
pub fn build_system(slots: &PromptSlots) -> String {
    let sections = [
        ("Identity", &slots.identity),
        ("Project Instructions", &slots.instructions),
        ("Memory Index", &slots.memory_index),
        ("Skills", &slots.skill_catalog),
        ("Tools", &slots.tool_note),
        ("Conversation Summary", &slots.summary),
    ];
    let mut out = String::new();
    for (title, body) in sections {
        if body.trim().is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str("# ");
        out.push_str(title);
        out.push('\n');
        out.push_str(body.trim());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_slots_emit_nothing() {
        assert_eq!(build_system(&PromptSlots::new()), "");
    }

    #[test]
    fn section_order_is_stable_and_empties_skip() {
        let slots = PromptSlots {
            summary: "s".to_string(),
            identity: "i".to_string(),
            ..PromptSlots::new()
        };
        let prompt = build_system(&slots);
        assert!(prompt.starts_with("# Identity\ni"));
        assert!(prompt.ends_with("# Conversation Summary\ns"));
        assert!(!prompt.contains("Skills"));
    }

    #[test]
    fn bodies_are_trimmed() {
        let slots = PromptSlots {
            identity: "  spaced  ".to_string(),
            ..PromptSlots::new()
        };
        assert_eq!(build_system(&slots), "# Identity\nspaced");
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        // Four CJK characters: byte cut would split one, char cut will not.
        let cut = truncate_to_budget("甲乙丙丁戊", 4);
        assert_eq!(cut, "甲乙丙丁...[truncated]");
        assert_eq!(truncate_to_budget("short", 10), "short");
    }
}
