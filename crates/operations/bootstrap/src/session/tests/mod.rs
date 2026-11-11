use std::sync::Mutex;

use runtime_runner::RunLoop;
use state_store::Conversation;

use super::*;
use crate::HeadlessDeny;
use crate::compactor::ContextCompactor;
use crate::hook_adapter::HookAdapter;
use crate::model_adapter::ModelAdapter;
use crate::plan_adapter::TodoPlanTracker;
use crate::policy_adapter::PolicyAdapter;
use crate::tool_adapter::ToolAdapter;
use wavecode_llm::{ChatModel, ChatRequest, EventStream, StreamEvent, Usage};
use wavecode_wire::{Op, Submission};

use super::assembly::seed_conversation;

const CONFIG: &str = r#"
model = "m1"
model_provider = "p1"

[model_providers.p1]
type = "anthropic"
base_url = "https://api.example.com/anthropic"
api_key = "k-inline"
"#;

/// Load a config built on the default fixture text.
fn config_with(tail: &str) -> wavecode_config::Config {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, format!("{CONFIG}{tail}")).unwrap();
    wavecode_config::Config::load_from(&path).unwrap()
}

fn displayed(rules: &[wavecode_sandbox::Rule]) -> Vec<String> {
    rules.iter().map(|rule| rule.to_string()).collect()
}

mod assembly;
mod journal;
mod permissions;
