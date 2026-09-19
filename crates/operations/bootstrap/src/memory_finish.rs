/*!
 * @file SessionMemoryFinish
 * @description Model-distilled session-end memory extraction behind the driver seam.
 *
 * Responsibilities:
 * - Distill durable memories from the final transcript with one model call.
 * - Append parsed entries through the memory store (index updated atomically).
 * - Decorate any turn driver with teardown extraction that never fails exit.
 *
 * This module must not be depended on by: runtime, state, action, safety,
 * capabilities, or any lower layer. It is composition-root code: the actor
 * only sees the `TurnDriver::end_session` seam, never these types.
 */

//! Shutdown extraction: transcript in, stored memories out, silence on failure.

use std::sync::Arc;

use runtime_runner::{HookPoint, InboxHandle, RunContext, StopReason, TurnDriver, TurnInput};
use state_store::{CompactTrigger, Conversation};
use wavecode_llm::{ChatModel, ChatRequest, ContentBlock, Message, Role};
use wavecode_memory::{MemoryStore, parse_extracted_entries};
use wavecode_wire::Event;

/// Distillation system prompt: line format in, line format out.
const DISTILL_SYSTEM: &str = "Extract durable memories from this session transcript for future sessions. \
Output one `[category] content` line per memory and nothing else. Categories: user (stable user preferences and facts), \
feedback (corrections and guidance the user gave), project (conventions of this codebase), \
reference (reusable facts and pointers). Skip ephemeral task detail, tool chatter, and anything already obvious. \
If nothing is worth keeping, output nothing.";

/// Transcript characters fed to distillation; bounds one extra model call.
const MAX_TRANSCRIPT_CHARS: usize = 8_000;

/// Output tokens for the distillation call: entries are short lines.
const DISTILL_MAX_TOKENS: u32 = 512;

/// Model-distilled memory extraction over a finished transcript.
pub struct MemoryFinisher {
    model: Arc<dyn ChatModel>,
    model_name: String,
    store: MemoryStore,
}

impl MemoryFinisher {
    /// Wrap a model channel and the store the prompt index reads.
    pub fn new(model: Arc<dyn ChatModel>, model_name: String, store: MemoryStore) -> Self {
        Self {
            model,
            model_name,
            store,
        }
    }

    /// Distill and store memories; returns stored entry count.
    ///
    /// Empty transcripts skip the model call entirely. Failures (model
    /// errors, IO errors) return `Err` and callers must swallow it:
    /// extraction is best-effort and never blocks session exit.
    pub async fn finish(&self, transcript: &[String]) -> Result<usize, String> {
        let body: String = transcript.join("\n");
        if body.trim().is_empty() {
            return Ok(0);
        }
        let mut text = body.chars().take(MAX_TRANSCRIPT_CHARS).collect::<String>();
        if text.len() < body.len() {
            text.push_str("\n[transcript truncated for extraction]");
        }
        let request = ChatRequest {
            model: self.model_name.clone(),
            system: DISTILL_SYSTEM.to_string(),
            messages: Arc::new(vec![Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text }],
            }]),
            tools: Vec::new(),
            max_tokens: DISTILL_MAX_TOKENS,
        };
        let mut stream = self
            .model
            .stream(request)
            .await
            .map_err(|e| format!("distillation sample failed: {e}"))?;
        let mut distilled = String::new();
        {
            use futures::StreamExt;
            while let Some(event) = stream.next().await {
                let event = event.map_err(|e| format!("distillation stream failed: {e}"))?;
                if let wavecode_llm::StreamEvent::TextDelta { text } = event {
                    distilled.push_str(&text);
                }
            }
        }
        let mut stored = 0;
        for (category, content) in parse_extracted_entries(&distilled) {
            self.store
                .append(category, &content)
                .map_err(|e| format!("memory store failed: {e}"))?;
            stored += 1;
        }
        // Background consolidation ("dream" merge): fold the near-duplicate
        // entries the store just grew. Best-effort under the same silence
        // contract as extraction — a merge failure must never block session
        // exit, and this layer has no event channel to log through (see
        // `end_session`), so failures are skipped.
        let _ = self.store.consolidate();
        Ok(stored)
    }
}

/// Turn driver decorator adding teardown extraction to any driver.
///
/// All turn-driving methods delegate untouched; `end_session` distills the
/// final transcript through the finisher. Extraction failures stay silent
/// by contract so shutdown timing never depends on model or disk health.
pub struct SessionMemory<D> {
    inner: D,
    finisher: Option<MemoryFinisher>,
}

impl<D> SessionMemory<D> {
    /// Wrap a driver; `None` finisher (memory disabled) only delegates.
    pub fn new(inner: D, finisher: Option<MemoryFinisher>) -> Self {
        Self { inner, finisher }
    }
}

#[async_trait::async_trait]
impl<D: TurnDriver> TurnDriver for SessionMemory<D> {
    async fn drive_turn(
        &self,
        ctx: &RunContext,
        conv: &mut Conversation,
        input: TurnInput<'_>,
        system: &str,
        on_event: &(dyn Fn(Event) + Send + Sync),
    ) -> StopReason {
        self.inner
            .drive_turn(ctx, conv, input, system, on_event)
            .await
    }

    async fn drive_compact(
        &self,
        conv: &mut Conversation,
        trigger: CompactTrigger,
        on_event: &(dyn Fn(Event) + Send + Sync),
    ) -> Result<(), String> {
        self.inner.drive_compact(conv, trigger, on_event).await
    }

    async fn drive_hook(
        &self,
        point: HookPoint,
        payload: &str,
        on_event: &(dyn Fn(Event) + Send + Sync),
    ) -> bool {
        self.inner.drive_hook(point, payload, on_event).await
    }

    fn set_permission_mode(&self, mode: &str) -> bool {
        self.inner.set_permission_mode(mode)
    }

    fn set_model(&self, name: &str) -> bool {
        self.inner.set_model(name)
    }

    fn set_thinking(&self, effort: &str) -> bool {
        self.inner.set_thinking(effort)
    }

    fn inbox_handle(&self) -> Option<InboxHandle> {
        self.inner.inbox_handle()
    }

    async fn end_session(&self, transcript: &[String]) {
        if let Some(finisher) = &self.finisher {
            // Best-effort by contract: the count is unobservable here by
            // design (no event channel at this layer), failures stay silent.
            let _ = finisher.finish(transcript).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wavecode_llm::EventStream;

    struct ScriptedModel {
        scripts: std::sync::Mutex<std::collections::VecDeque<String>>,
    }

    #[async_trait::async_trait]
    impl ChatModel for ScriptedModel {
        async fn stream(&self, _req: ChatRequest) -> wavecode_llm::Result<EventStream> {
            let text = self
                .scripts
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pop_front()
                .unwrap_or_default();
            Ok(Box::pin(futures::stream::iter(vec![Ok(
                wavecode_llm::StreamEvent::TextDelta { text },
            )])))
        }
    }

    fn finisher(output: &str) -> (MemoryFinisher, tempfile::TempDir, Arc<ScriptedModel>) {
        let dir = tempfile::tempdir().unwrap();
        let model = Arc::new(ScriptedModel {
            scripts: std::sync::Mutex::new([output.to_string()].into_iter().collect()),
        });
        (
            MemoryFinisher::new(
                model.clone(),
                "scripted".to_string(),
                MemoryStore::new(dir.path().to_path_buf()),
            ),
            dir,
            model,
        )
    }

    #[tokio::test]
    async fn distilled_lines_store_by_category() {
        let (finisher, dir, _) =
            finisher("[user] prefers tabs\n[project] uses trunk-based flow\nnoise without a tag\n");
        let stored = finisher
            .finish(&["user: remember my tabs".to_string()])
            .await
            .unwrap();
        assert_eq!(stored, 2);
        let store = MemoryStore::new(dir.path().to_path_buf());
        assert!(store.read_index().unwrap().contains("[user]"));
        assert!(
            store
                .read_category(wavecode_memory::MemoryCategory::Project)
                .unwrap()
                .contains("trunk-based")
        );
    }

    #[tokio::test]
    async fn empty_transcripts_skip_the_model_call() {
        let (finisher, _dir, model) = finisher("[user] should never be consumed");
        assert_eq!(finisher.finish(&[]).await.unwrap(), 0);
        assert_eq!(finisher.finish(&["   ".to_string()]).await.unwrap(), 0);
        // Nothing stored, and the script is unconsumed (no call happened).
        assert_eq!(model.scripts.lock().unwrap().len(), 1);
    }
}
