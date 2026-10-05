/*!
 * @file EvictingGateway
 * @description Cache-preserving tool-result eviction at the gateway seam.
 *
 * Responsibilities:
 * - Estimate per-request usage and fire the shared eviction pass once the
 *   soft threshold is crossed.
 * - Replace old tool-result payloads with deterministic stubs while the
 *   anchored prefix and the recent window stay untouched.
 * - Delegate every request (evicted or not) unchanged to the inner
 *   gateway, including streaming and model switching.
 *
 * This module must not depend on: runtime internals beyond its trait seam.
 */

//! [`runtime_runner::ModelGateway`] decorator over the shared
//! cache-preserving micro-compaction pass in `wavecode-context`.

use runtime_runner::{ModelGateway, SampleDelta, SampleError, SampleRequest, SampleResponse};
use state_store::{
    Block, CONTEXT_OVERHEAD_TOKENS, HistoryEntry, Role as HistoryRole, estimate_tokens,
};
use wavecode_context::{EvictionConfig, evict_old_tool_results, should_evict_tool_results};
use wavecode_llm::{ContentBlock, Message, Role};

use crate::model_adapter::to_content_block;

/// Gateway decorator replacing old tool-result payloads with stubs.
///
/// The pass runs on the request snapshot only: stored history keeps the
/// original payloads, so a resumed session re-derives the same stubs from
/// the same deterministic function. Below the soft threshold the request
/// passes through byte-for-byte, keeping the provider prompt cache prefix
/// stable.
pub struct EvictingGateway<G> {
    inner: G,
    cfg: EvictionConfig,
}

impl<G> EvictingGateway<G> {
    /// Wrap `inner` with the default eviction policy.
    pub fn new(inner: G) -> Self {
        Self::with_config(inner, EvictionConfig::default())
    }

    /// Wrap `inner` with an explicit eviction policy.
    pub fn with_config(inner: G, cfg: EvictionConfig) -> Self {
        Self { inner, cfg }
    }

    /// Estimated request usage: the fallback char estimator over system
    /// and history text plus framing overhead — the same numbers the rest
    /// of the loop reasons with when no provider usage has arrived yet.
    fn usage_estimate(request: &SampleRequest) -> u64 {
        let mut text = String::with_capacity(1024);
        text.push_str(&request.system);
        for entry in &request.messages {
            text.push('\n');
            text.push_str(&entry.text());
        }
        estimate_tokens(&text) + CONTEXT_OVERHEAD_TOKENS
    }

    /// Apply the eviction pass when the soft threshold is crossed. The
    /// pass is idempotent (stubs are pure functions of the call id and
    /// tool name) and leaves short histories unchanged. Consumes the
    /// request: the caller holds no other reference, so the pass-through
    /// path needs no clone at all.
    fn evicted(&self, mut request: SampleRequest) -> SampleRequest {
        if !should_evict_tool_results(Self::usage_estimate(&request), &self.cfg) {
            return request;
        }
        let messages: Vec<Message> = request.messages.iter().map(to_message).collect();
        let evicted = evict_old_tool_results(&messages, &self.cfg);
        request.messages = evicted.iter().map(from_message).collect();
        request
    }
}

/// Convert one history entry to the provider message shape.
fn to_message(entry: &HistoryEntry) -> Message {
    Message {
        role: if entry.role == HistoryRole::Assistant {
            Role::Assistant
        } else {
            Role::User
        },
        content: entry.blocks.iter().map(to_content_block).collect(),
    }
}

/// Map one provider block back onto the canonical history block.
fn from_content_block(block: &ContentBlock) -> Block {
    match block {
        ContentBlock::Text { text } => Block::Text(text.clone()),
        ContentBlock::Image { id, mime, base64 } => Block::Image {
            id: id.clone(),
            mime: mime.clone(),
            base64: base64.clone(),
        },
        ContentBlock::ToolUse { id, name, input } => Block::ToolUse {
            call_id: id.clone(),
            name: name.clone(),
            input: input.clone(),
        },
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => Block::ToolResult {
            call_id: tool_use_id.clone(),
            content: content.clone(),
            is_error: *is_error,
            // The provider block shape carries no wall-clock stamp, and
            // this rebuild feeds a request snapshot only: the stored
            // history keeps its original stamped result.
            produced_at: None,
        },
        ContentBlock::Thinking { text, signature } => Block::Thinking {
            text: text.clone(),
            signature: signature.clone(),
        },
    }
}

/// Convert one provider message back onto the history shape.
fn from_message(message: &Message) -> HistoryEntry {
    HistoryEntry {
        role: if message.role == Role::Assistant {
            HistoryRole::Assistant
        } else {
            HistoryRole::User
        },
        blocks: message.content.iter().map(from_content_block).collect(),
    }
}

#[async_trait::async_trait]
impl<G: ModelGateway + Send + Sync> ModelGateway for EvictingGateway<G> {
    async fn sample(&self, request: SampleRequest) -> Result<SampleResponse, SampleError> {
        let request = self.evicted(request);
        self.inner.sample(request).await
    }

    async fn sample_streaming(
        &self,
        request: SampleRequest,
        on_delta: &(dyn Fn(SampleDelta) + Send + Sync),
    ) -> Result<SampleResponse, SampleError> {
        let request = self.evicted(request);
        self.inner.sample_streaming(request, on_delta).await
    }

    fn set_model(&self, name: &str) -> bool {
        self.inner.set_model(name)
    }

    fn context_window(&self) -> Option<u64> {
        self.inner.context_window()
    }

    fn current_model(&self) -> Option<String> {
        self.inner.current_model()
    }

    fn set_thinking(&self, effort: &str) -> bool {
        self.inner.set_thinking(effort)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::sync::Mutex;

    /// Records every request it receives; never fails.
    #[derive(Default)]
    struct RecordingGateway {
        seen: Mutex<Vec<SampleRequest>>,
    }

    impl RecordingGateway {
        fn requests(&self) -> Vec<SampleRequest> {
            self.seen.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }
    }

    #[async_trait::async_trait]
    impl ModelGateway for RecordingGateway {
        async fn sample(&self, request: SampleRequest) -> Result<SampleResponse, SampleError> {
            self.seen
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(request);
            Ok(SampleResponse::default())
        }
    }

    fn user_entry(text: &str) -> HistoryEntry {
        HistoryEntry {
            role: HistoryRole::User,
            blocks: vec![Block::Text(text.to_string())],
        }
    }

    fn tool_pair(call_id: &str, name: &str, payload: &str) -> (HistoryEntry, HistoryEntry) {
        (
            HistoryEntry {
                role: HistoryRole::Assistant,
                blocks: vec![Block::ToolUse {
                    call_id: call_id.to_string(),
                    name: name.to_string(),
                    input: Value::Null,
                }],
            },
            HistoryEntry {
                role: HistoryRole::User,
                blocks: vec![Block::ToolResult {
                    call_id: call_id.to_string(),
                    content: payload.to_string(),
                    is_error: false,
                    produced_at: None,
                }],
            },
        )
    }

    /// Well under any threshold, and stepping one message at a time so the
    /// fixtures can assert on individual stubs (the shipped default batches).
    fn tiny_config() -> EvictionConfig {
        EvictionConfig {
            anchored_prefix: 1,
            recent_window: 2,
            // nosemgrep: codacy.yaml.security.hard-coded-tokens
            soft_threshold_tokens: 10,
            batch_messages: 1,
        }
    }

    #[tokio::test]
    async fn below_threshold_passes_through_unchanged() {
        let inner = RecordingGateway::default();
        let gateway = EvictingGateway::with_config(
            inner,
            EvictionConfig {
                // nosemgrep: codacy.yaml.security.hard-coded-tokens
                soft_threshold_tokens: u64::MAX,
                ..EvictionConfig::default()
            },
        );
        let request = SampleRequest {
            system: String::new(),
            messages: vec![user_entry("hello")],
            tools: vec![],
            output_cap: 0,
            notes: Vec::new(),
        };
        gateway.sample(request.clone()).await.unwrap();
        assert_eq!(gateway.inner.requests(), vec![request]);
    }

    #[tokio::test]
    async fn old_tool_results_get_stubbed_recent_ones_survive() {
        let (old_call, old_result) = tool_pair("c1", "shell", "old payload");
        let (recent_call, recent_result) = tool_pair("c2", "shell", "fresh payload");
        let entries = vec![
            user_entry("head"),
            old_call,
            old_result,
            user_entry("middle"),
            recent_call,
            recent_result,
            user_entry("tail"),
        ];
        let gateway = EvictingGateway::with_config(RecordingGateway::default(), tiny_config());
        let request = SampleRequest {
            system: String::new(),
            messages: entries,
            tools: vec![],
            output_cap: 0,
            notes: Vec::new(),
        };
        gateway.sample(request.clone()).await.unwrap();
        let seen = gateway.inner.requests();
        assert_eq!(seen.len(), 1);
        // Head (anchored prefix) untouched.
        assert_eq!(seen[0].messages[0].text(), "head");
        // The old result became the deterministic stub, named after its
        // call; the recent-window result kept its payload.
        let rendered: Vec<String> = seen[0].messages.iter().map(|e| e.text()).collect();
        assert!(
            rendered
                .iter()
                .any(|t| t.contains("[evicted tool result for shell (c1)]")),
            "old result stubbed: {rendered:?}"
        );
        assert!(
            rendered.iter().any(|t| t.contains("fresh payload")),
            "recent result intact: {rendered:?}"
        );
    }

    #[tokio::test]
    async fn eviction_is_idempotent_across_samples() {
        let (call, result) = tool_pair("c1", "shell", "payload");
        let entries = vec![user_entry("head"), call, result, user_entry("tail")];
        let gateway = EvictingGateway::with_config(RecordingGateway::default(), tiny_config());
        let request = SampleRequest {
            system: String::new(),
            messages: entries,
            tools: vec![],
            output_cap: 0,
            notes: Vec::new(),
        };
        gateway.sample(request.clone()).await.unwrap();
        gateway.sample(request).await.unwrap();
        let seen = gateway.inner.requests();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].messages, seen[1].messages);
    }

    #[test]
    fn set_model_delegates_to_inner() {
        let gateway = EvictingGateway::new(RecordingGateway::default());
        // The recording inner keeps the rejecting default.
        assert!(!gateway.set_model("other"));
    }

    #[test]
    fn image_blocks_round_trip_through_the_eviction_shape() {
        let entry = HistoryEntry {
            role: HistoryRole::User,
            blocks: vec![Block::Image {
                id: Some("shot".to_string()),
                mime: "image/png".to_string(),
                base64: "aGk=".to_string(),
            }],
        };
        let message = to_message(&entry);
        assert_eq!(from_message(&message), entry);
        assert!(entry.text().contains("[image image/png: shot]"));
    }
}
