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

/// Character budget split between the system prompt and history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    /// Total characters available for system plus history.
    pub total_chars: usize,
    /// Characters reserved for history; the system gets the rest.
    pub reserved_for_history: usize,
}

impl Budget {
    /// Characters the system prompt may occupy (saturates at zero).
    pub fn system_chars(&self) -> usize {
        self.total_chars.saturating_sub(self.reserved_for_history)
    }
}

/// All candidate slots before budgeting.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceBundle {
    /// Agent identity and operating rules.
    pub identity: String,
    /// Project instructions.
    pub instructions: String,
    /// Persistent memory index snapshot.
    pub memory_index: String,
    /// Full skill catalog before truncation.
    pub skill_catalog: String,
    /// Tool availability note.
    pub tool_note: String,
    /// Compaction summary carried from the previous window.
    pub summary: String,
}

/// Budgeted assembly result with honest accounting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assembled {
    /// Final system prompt text.
    pub system: String,
    /// Characters consumed by the system prompt.
    pub used_chars: usize,
    /// True when the skill catalog was truncated to fit.
    pub catalog_truncated: bool,
    /// Section titles dropped for space, in drop order.
    pub dropped: Vec<&'static str>,
}

/// Assemble a system prompt inside a character budget.
///
/// Fixed sections keep a priority order (identity, summary, tools,
/// instructions, memory); the skill catalog fills whatever remains.
/// Over-budget fixed sections drop lowest-priority first and report
/// every drop — callers never silently lose instructions.
pub fn assemble_budgeted(bundle: &SourceBundle, budget: &Budget) -> Assembled {
    let mut available = budget.system_chars();
    // Fixed sections in keep-priority order with their slot titles.
    let fixed: &[(&str, &str)] = &[
        ("Identity", &bundle.identity),
        ("Conversation Summary", &bundle.summary),
        ("Tools", &bundle.tool_note),
        ("Project Instructions", &bundle.instructions),
        ("Memory Index", &bundle.memory_index),
    ];
    let mut kept: Vec<(&str, String)> = Vec::new();
    let mut dropped = Vec::new();
    // Fill highest-priority first so drops always hit the least
    // important surviving section.
    for (title, body) in fixed {
        let body = body.trim();
        if body.is_empty() {
            continue;
        }
        let cost = body.chars().count() + title.len() + 3;
        if cost <= available {
            available -= cost;
            kept.push((title, body.to_string()));
        } else {
            dropped.push(*title);
        }
    }
    // The catalog takes the leftovers, truncated with a marker.
    let catalog = bundle.skill_catalog.trim();
    let mut catalog_truncated = false;
    if !catalog.is_empty() {
        if catalog.chars().count() + 10 <= available {
            kept.push(("Skills", catalog.to_string()));
        } else if available > 20 {
            kept.push(("Skills", truncate_to_budget(catalog, available - 20)));
            catalog_truncated = true;
        } else {
            dropped.push("Skills");
        }
    }
    let mut system = String::new();
    for (title, body) in &kept {
        if !system.is_empty() {
            system.push_str("\n\n");
        }
        system.push_str("# ");
        system.push_str(title);
        system.push('\n');
        system.push_str(body);
    }
    let used_chars = system.chars().count();
    Assembled {
        system,
        used_chars,
        catalog_truncated,
        dropped,
    }
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

    fn bundle() -> SourceBundle {
        SourceBundle {
            identity: "i".to_string(),
            instructions: "do good".to_string(),
            memory_index: "m".to_string(),
            skill_catalog: "s1 s2 s3".to_string(),
            tool_note: "tools: a".to_string(),
            summary: String::new(),
        }
    }

    #[test]
    fn budgeted_assembly_fits_everything_with_room() {
        let assembled = assemble_budgeted(
            &bundle(),
            &Budget {
                total_chars: 10_000,
                reserved_for_history: 8_000,
            },
        );
        assert!(!assembled.catalog_truncated);
        assert!(assembled.dropped.is_empty());
        assert!(assembled.system.contains("# Identity"));
        assert!(assembled.used_chars <= 2000);
    }

    #[test]
    fn tight_budgets_truncate_catalog_then_drop_low_priority() {
        // Room for identity + tools only; instructions and memory drop.
        let assembled = assemble_budgeted(
            &bundle(),
            &Budget {
                total_chars: 30,
                reserved_for_history: 0,
            },
        );
        assert!(assembled.system.contains("# Identity"));
        assert!(assembled.dropped.contains(&"Memory Index"));
        // Starvation keeps the result honest: drops are reported.
        let starved = assemble_budgeted(
            &bundle(),
            &Budget {
                total_chars: 2,
                reserved_for_history: 0,
            },
        );
        assert!(!starved.system.contains("# Identity"));
        assert!(starved.dropped.contains(&"Identity"));
    }
}
