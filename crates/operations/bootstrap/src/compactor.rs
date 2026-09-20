/*!
 * @file ModelCompactor
 * @description Summarizing compactor behind the Compactor seam.
 *
 * Responsibilities:
 * - Convert seam messages to provider messages and back.
 * - Run the shared compact_history pipeline with model summaries.
 * - Append the post-compaction footers that keep a long session continuable:
 *   a pointer to the on-disk turn journal (exact output stays retrievable)
 *   and the live task list (attached from its source, never transcribed).
 * - Report pipeline failures as seam errors.
 *
 * This module must not depend on: runtime internals beyond its trait seam.
 */

//! [`runtime_runner::Compactor`] implemented over `wavecode-context`.

use std::path::PathBuf;
use std::sync::Arc;

use runtime_runner::{CompactError, Compacted, Compactor};
use state_store::{HistoryEntry, Role as HistoryRole, estimate_tokens};
use wavecode_context::{ContextConfig, ModelSummary, compact_history};
use wavecode_llm::{ChatModel, ContentBlock, Message, Role};

/// Facts the footers need, captured at assembly time.
#[derive(Debug, Clone, Default)]
pub struct CompactionFooters {
    /// Turn journal of the session being compacted, when one exists. Its
    /// records hold the whole dialogue (tool payloads included) as text, so a
    /// post-compaction turn can look up exact output instead of guessing.
    pub journal: Option<PathBuf>,
}

/// Compactor summarizing through the shared model channel.
pub struct ContextCompactor {
    model: Arc<dyn ChatModel>,
    model_name: String,
    footers: CompactionFooters,
    /// Live task list, re-attached after a summary so plan state survives
    /// compaction without the summarizer transcribing it.
    plans: Option<wavecode_tools::TodoStore>,
}

impl ContextCompactor {
    /// Summarize with the same model that samples turns.
    pub fn new(model: Arc<dyn ChatModel>, model_name: String) -> Self {
        Self {
            model,
            model_name,
            footers: CompactionFooters::default(),
            plans: None,
        }
    }

    /// Attach the journal pointer appended after every summary.
    pub fn with_footers(mut self, footers: CompactionFooters) -> Self {
        self.footers = footers;
        self
    }

    /// Attach the live task list (the same store the todo tool writes) so it
    /// can be re-attached to the post-compaction history.
    pub fn with_plans(mut self, plans: wavecode_tools::TodoStore) -> Self {
        self.plans = Some(plans);
        self
    }

    /// Compose the final summary text: the model's summary plus the footers.
    fn finalize(&self, summary: String) -> String {
        let mut text = summary;
        if let Some(journal) = &self.footers.journal {
            text.push_str("\n\n");
            text.push_str(&recovery_footer(journal));
        }
        if let Some(plans) = &self.plans
            && let Some(footer) = plan_footer(&plans.snapshot())
        {
            text.push_str("\n\n");
            text.push_str(&footer);
        }
        text
    }
}

/// Pointer appended after a summary: where the compacted conversation still
/// lives and how to read it. This is what a long-horizon session needs most
/// after compaction — the ability to look up exact output it no longer holds.
///
/// When the session has spawned subagent tasks, their own journals exist
/// beside this one; the parent's records hold only the spawn call and the
/// child's returned summary, so the note names the children directory too.
pub fn recovery_footer(journal: &std::path::Path) -> String {
    let mut text = format!(
        "## Context Recovery\n\
         Earlier turns of this session are on disk in its turn journal:\n  {}\n\
         One JSON record per line in turn order (newest last); each record holds \
         `input`, the full `history` of that turn with tool calls and results \
         rendered as text, and the turn's `outcome`. If you need exact command \
         output, file contents, or error text from before this compaction, grep \
         that file for a keyword, then read the matching line instead of guessing \
         or re-running.",
        journal.display()
    );
    if let Some(children) = children_dir_with_logs(journal) {
        text.push_str(&format!(
            "\nSubagent (`task`) steps are NOT in this file: each child task logs \
             its own turns under\n  {}\n\
             (one `<child-id>.jsonl` per task), while a parent record keeps only the \
             spawn call and the child's returned summary. Check there for a task's \
             exact steps.",
            children.display()
        ));
    }
    text
}

/// The session's children-journal directory when it actually holds logs;
/// `None` otherwise so the footer stays silent about subagents that never ran.
fn children_dir_with_logs(journal: &std::path::Path) -> Option<std::path::PathBuf> {
    let stem = journal.file_stem()?;
    let dir = journal
        .parent()?
        .join("children")
        .join(stem.to_string_lossy().as_ref());
    let has_logs = std::fs::read_dir(&dir)
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false);
    has_logs.then_some(dir)
}

/// Live task list attached after a summary; `None` when the list is empty
/// (nothing to preserve, and an empty heading would only add noise).
pub fn plan_footer(items: &[wavecode_tools::TodoItem]) -> Option<String> {
    if items.is_empty() {
        return None;
    }
    Some(format!(
        "## Live plan (attached from the task list at compaction time; \
         authoritative over anything above)\n{}",
        wavecode_tools::format_todos(items)
    ))
}

#[async_trait::async_trait]
impl Compactor for ContextCompactor {
    async fn compact(
        &self,
        history: Vec<HistoryEntry>,
        trigger: state_store::CompactTrigger,
    ) -> Result<Compacted, CompactError> {
        // The summarizer works on prose: block entries flatten to their
        // legacy text view so tool payloads stay visible to the summary.
        let messages: Vec<Message> = history
            .into_iter()
            .filter(|entry| !entry.text().is_empty())
            .map(|entry| Message {
                role: if entry.role == HistoryRole::Assistant {
                    Role::Assistant
                } else {
                    Role::User
                },
                content: vec![ContentBlock::Text { text: entry.text() }],
            })
            .collect();
        let mut strategy = ModelSummary::new(self.model.clone(), self.model_name.clone());
        // Manual compaction may carry user steering ("/compact keep the
        // API design decisions"); other triggers summarize unsteered.
        if let state_store::CompactTrigger::Manual {
            instruction: Some(focus),
        } = &trigger
        {
            strategy = strategy.with_focus(focus.clone());
        }
        let outcome = compact_history(&messages, &strategy, &ContextConfig::default())
            .await
            .map_err(|e| CompactError::Failed(e.to_string()))?;
        let summary = self.finalize(outcome.summary);
        Ok(Compacted {
            summary_tokens: estimate_tokens(&summary),
            summary,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_footer_names_the_journal_and_how_to_read_it() {
        let footer = recovery_footer(std::path::Path::new("/home/u/.wavecode/sessions/s-1.jsonl"));
        assert!(footer.starts_with("## Context Recovery"), "{footer}");
        assert!(
            footer.contains("/home/u/.wavecode/sessions/s-1.jsonl"),
            "{footer}"
        );
        assert!(footer.contains("grep"), "{footer}");
        // No children logs on disk: the footer says nothing about subagents.
        assert!(!footer.contains("Subagent"), "{footer}");
    }

    /// Child-task logs beside the session journal switch on the subagent note,
    /// pointing at the directory a post-compaction turn should search.
    #[test]
    fn recovery_footer_points_at_child_journals_when_they_exist() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join(".wavecode").join("sessions");
        let journal = sessions.join("s-1.jsonl");
        std::fs::create_dir_all(&journal).unwrap();
        let children = sessions.join("children").join("s-1");
        std::fs::create_dir_all(&children).unwrap();
        std::fs::write(children.join("child-1.jsonl"), b"{\"format\":1}\n").unwrap();

        let footer = recovery_footer(&journal);
        assert!(footer.contains("Subagent"), "{footer}");
        assert!(footer.contains("jsonl"), "{footer}");
        assert!(footer.contains(&children.display().to_string()), "{footer}");

        // An empty children directory stays silent (no subagent ever ran).
        std::fs::remove_file(children.join("child-1.jsonl")).unwrap();
        let bare = recovery_footer(&journal);
        assert!(!bare.contains("Subagent"), "{bare}");
    }

    #[test]
    fn plan_footer_skips_an_empty_list() {
        assert!(plan_footer(&[]).is_none());
        let items = vec![wavecode_tools::TodoItem {
            id: "1".to_string(),
            content: "ship it".to_string(),
            status: wavecode_tools::TodoStatus::InProgress,
        }];
        let footer = plan_footer(&items).expect("non-empty list gets a footer");
        assert!(footer.contains("## Live plan"), "{footer}");
        assert!(footer.contains("ship it"), "{footer}");
    }

    #[test]
    fn finalize_appends_journal_then_plan() {
        let store = wavecode_tools::TodoStore::default();
        store.write(vec![wavecode_tools::TodoItem {
            id: "7".to_string(),
            content: "keep going".to_string(),
            status: wavecode_tools::TodoStatus::Pending,
        }]);
        let compactor = ContextCompactor::new(Arc::new(NoopModel), "m".to_string())
            .with_footers(CompactionFooters {
                journal: Some(PathBuf::from("/tmp/j.jsonl")),
            })
            .with_plans(store);
        let text = compactor.finalize("## Goal\nship".to_string());
        let recovery = text.find("## Context Recovery").expect("recovery footer");
        let plan = text.find("## Live plan").expect("plan footer");
        assert!(recovery < plan, "{text}");
        assert!(text.starts_with("## Goal\nship"), "{text}");
    }

    /// The finalizer is pure text composition; the model is never called.
    struct NoopModel;

    #[async_trait::async_trait]
    impl ChatModel for NoopModel {
        async fn stream(
            &self,
            _req: wavecode_llm::ChatRequest,
        ) -> wavecode_llm::Result<wavecode_llm::EventStream> {
            unreachable!("finalize never samples")
        }

        fn set_thinking(&self, _effort: &str) -> bool {
            false
        }
    }
}
