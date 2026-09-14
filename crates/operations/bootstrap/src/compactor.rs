/*!
 * @file ModelCompactor
 * @description Summarizing compactor behind the Compactor seam.
 *
 * Responsibilities:
 * - Convert seam messages to provider messages and back.
 * - Run the shared compact_history pipeline with model summaries.
 * - Report pipeline failures as seam errors.
 *
 * This module must not depend on: runtime internals beyond its trait seam.
 */

//! [`runtime_runner::Compactor`] implemented over `wavecode-context`.

use std::sync::Arc;

use runtime_runner::{CompactError, Compacted, Compactor};
use state_store::{HistoryEntry, Role as HistoryRole, estimate_tokens};
use wavecode_context::{ContextConfig, ModelSummary, compact_history};
use wavecode_llm::{ChatModel, ContentBlock, Message, Role};

/// Compactor summarizing through the shared model channel.
pub struct ContextCompactor {
    model: Arc<dyn ChatModel>,
    model_name: String,
}

impl ContextCompactor {
    /// Summarize with the same model that samples turns.
    pub fn new(model: Arc<dyn ChatModel>, model_name: String) -> Self {
        Self { model, model_name }
    }
}

#[async_trait::async_trait]
impl Compactor for ContextCompactor {
    async fn compact(
        &self,
        history: Vec<HistoryEntry>,
        _trigger: state_store::CompactTrigger,
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
        let strategy = ModelSummary::new(self.model.clone(), self.model_name.clone());
        let outcome = compact_history(&messages, &strategy, &ContextConfig::default())
            .await
            .map_err(|e| CompactError::Failed(e.to_string()))?;
        Ok(Compacted {
            summary_tokens: estimate_tokens(&outcome.summary),
            summary: outcome.summary,
        })
    }
}
