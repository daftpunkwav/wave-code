/*!
 * @file GatewayTestStubs
 * @description Scripted model and config shared by the gateway servers'
 * test modules.
 *
 * Responsibilities:
 * - Serve queued `StreamEvent` scripts, then plain completions so
 *   follow-up samples always terminate the loop.
 * - Provide the minimal provider config the hermetic assemblies load.
 *
 * This module is compiled for tests only (`cfg(test)`) and must never
 * ship; it must not depend on any server in this crate.
 */

//! Stub model plus config for the gateway servers' tests.

use std::sync::Mutex;

use wavecode_llm::{ChatModel, ChatRequest, EventStream, StreamEvent, Usage};

/// Minimal provider config the hermetic assemblies load (inline api key,
/// example base URL; nothing leaves the process).
pub(crate) const CONFIG: &str = r#"
model = "m1"
model_provider = "p1"

[model_providers.p1]
type = "anthropic"
base_url = "https://api.example.com/anthropic"
api_key = "k-inline"
"#;

/// One turn of plain text ending with `end_turn`: the default answer
/// once a scripted model's queued script runs out.
pub(crate) fn done_script() -> Vec<StreamEvent> {
    vec![
        StreamEvent::TextDelta {
            text: "done".to_string(),
        },
        StreamEvent::MessageComplete {
            stop_reason: "end_turn".to_string(),
            usage: Usage {
                // nosemgrep: Semgrep_codacy.yaml.security.hard-coded-tokens
                input_tokens: 1,
                // nosemgrep: Semgrep_codacy.yaml.security.hard-coded-tokens
                output_tokens: 1,
                ..Usage::default()
            },
        },
    ]
}

/// Scripted model: serves one queued script, then plain completions.
pub(crate) struct OneShotModel {
    script: Mutex<Option<Vec<StreamEvent>>>,
}

impl OneShotModel {
    /// Queue one script; the first sample consumes it.
    pub(crate) fn new(script: Vec<StreamEvent>) -> Self {
        Self {
            script: Mutex::new(Some(script)),
        }
    }
}

#[async_trait::async_trait]
impl ChatModel for OneShotModel {
    async fn stream(&self, _req: ChatRequest) -> wavecode_llm::Result<EventStream> {
        let script = self
            .script
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .unwrap_or_else(done_script);
        Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
    }
}
