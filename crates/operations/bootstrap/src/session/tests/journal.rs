//! The block journal: seed baselining, lost tool calls closing on
//! resume, gap repair, and the end-to-end scripted run over a real
//! journal.

use super::*;

/// The text seed is journaled as its baseline, so a session created
/// before the block journal existed does not end up with a journal that
/// describes only its tail.
#[test]
fn a_text_seed_becomes_the_journal_baseline() {
    let dir = tempfile::tempdir().unwrap();
    let journal = state_persistence::history::HistoryJournal::new(dir.path().join("h.jsonl"));
    let seed = vec![
        (false, "fix the test".to_string()),
        (true, "done".to_string()),
    ];
    let mut warnings = Vec::new();
    let conv = seed_conversation(Some(journal.clone()), &seed, &mut warnings);
    assert_eq!(conv.len(), 2);
    assert!(warnings.is_empty(), "{warnings:?}");

    // Resume once: the journal is now the source, and re-resuming must
    // not lengthen the history.
    for _ in 0..2 {
        let mut warnings = Vec::new();
        let conv = seed_conversation(Some(journal.clone()), &seed, &mut warnings);
        assert_eq!(conv.len(), 2, "resume grew the history");
        assert!(warnings.is_empty(), "{warnings:?}");
    }
}

/// A call whose result never reached disk closes as unresolved, and the
/// human is told instead of the gap being papered over.
#[test]
fn a_lost_tool_call_closes_and_warns_on_resume() {
    let dir = tempfile::tempdir().unwrap();
    let journal = state_persistence::history::HistoryJournal::new(dir.path().join("h.jsonl"));
    {
        let mut conversation = Conversation::with_sink(std::sync::Arc::new(
            crate::history_journal::JournalSink::new(journal.clone(), 0),
        ));
        conversation.push(state_store::Role::User, "write it");
        conversation.push_blocks(
            state_store::Role::Assistant,
            vec![state_store::Block::ToolUse {
                call_id: "c1".to_string(),
                name: "write".to_string(),
                input: serde_json::json!({"path": "a.txt"}),
            }],
        );
        // process dies before the result lands
    }
    let mut warnings = Vec::new();
    let conv = seed_conversation(Some(journal), &[], &mut warnings);
    assert_eq!(conv.len(), 3, "the closing result is appended");
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("lost their outcome"), "{warnings:?}");
    assert!(warnings[0].contains("c1"), "{warnings:?}");
}

/// No journal means the previous behavior exactly: the caller's seed is
/// the whole story, held in memory only.
#[test]
fn without_a_journal_the_seed_is_all_there_is() {
    let mut warnings = Vec::new();
    let conv = seed_conversation(
        None,
        &[(false, "a".to_string()), (true, "b".to_string())],
        &mut warnings,
    );
    assert_eq!(conv.len(), 2);
    assert!(warnings.is_empty(), "{warnings:?}");
}

/// Scripted model replaying canned streams in order; exhausted scripts
/// degrade to an empty completion so the loop always terminates.
struct ScriptedModel {
    scripts: std::sync::Mutex<std::collections::VecDeque<Vec<StreamEvent>>>,
}

#[async_trait::async_trait]
impl ChatModel for ScriptedModel {
    async fn stream(&self, _req: ChatRequest) -> wavecode_llm::Result<EventStream> {
        let script = self
            .scripts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
            .unwrap_or_else(|| {
                vec![StreamEvent::MessageComplete {
                    stop_reason: "end_turn".to_string(),
                    usage: Usage {
                        // nosemgrep: codacy.yaml.security.hard-coded-tokens
                        input_tokens: 1,
                        // nosemgrep: codacy.yaml.security.hard-coded-tokens
                        output_tokens: 1,
                        ..Usage::default()
                    },
                }]
            });
        Ok(Box::pin(futures::stream::iter(script.into_iter().map(Ok))))
    }
}

fn write_turn_script() -> Vec<StreamEvent> {
    vec![
        StreamEvent::TextDelta {
            text: "working".to_string(),
        },
        StreamEvent::ToolUseBegin {
            id: "c1".to_string(),
            name: "write".to_string(),
        },
        StreamEvent::ToolUseInputDelta {
            partial_json: r#"{"path":"hello.txt","content":"wavecode-smoke-ok"}"#.to_string(),
        },
        StreamEvent::BlockEnd,
        StreamEvent::MessageComplete {
            stop_reason: "end_turn".to_string(),
            usage: Usage {
                // nosemgrep: codacy.yaml.security.hard-coded-tokens
                input_tokens: 10,
                // nosemgrep: codacy.yaml.security.hard-coded-tokens
                output_tokens: 5,
                ..Usage::default()
            },
        },
    ]
}

fn done_script() -> Vec<StreamEvent> {
    vec![
        StreamEvent::TextDelta {
            text: "done".to_string(),
        },
        StreamEvent::MessageComplete {
            stop_reason: "end_turn".to_string(),
            usage: Usage {
                // nosemgrep: codacy.yaml.security.hard-coded-tokens
                input_tokens: 10,
                // nosemgrep: codacy.yaml.security.hard-coded-tokens
                output_tokens: 2,
                ..Usage::default()
            },
        },
    ]
}

/// End-to-end ReAct proof without network: a scripted model drives two
/// rounds (write, then report) through the real policy, real approval
/// bypass, and real filesystem tools in an isolated directory.
///
/// This is the closest offline stand-in for a live coding task: the
/// only substitution is the model itself. Live runs additionally need
/// a reachable provider; everything past sampling is identical.
#[tokio::test]
async fn scripted_loop_writes_a_real_file_across_rounds() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_path_buf();
    let (registry, todos) = wavecode_tools::Registry::builtin_with_todos();
    let registry = Arc::new(registry);
    let tools = ToolAdapter::new(
        registry.clone(),
        wavecode_tools::ToolCtx {
            cwd: cwd.clone(),
            deny_env: Vec::new(),
        },
    );
    let policy = PolicyAdapter::new(
        wavecode_sandbox::Sandbox::without_rules(wavecode_protocol::PermissionMode::Auto),
        registry.clone(),
    );
    let hooks = HookAdapter::new(
        Arc::new(wavecode_hooks::HookEngine::new(
            std::collections::HashMap::new(),
        )),
        cwd.clone(),
    );
    let model = Arc::new(ScriptedModel {
        scripts: std::sync::Mutex::new([write_turn_script(), done_script()].into_iter().collect()),
    });
    let adapter = ModelAdapter::new(model.clone(), "scripted".to_string(), 100, registry.clone());
    let plans = TodoPlanTracker::new(todos);
    let compactor = ContextCompactor::new(model.clone(), "scripted".to_string());
    let interrupt = infrastructure_base::InterruptHandle::new();
    // Durability rides the same conversation a live session would use:
    // the journal is attached at construction, so every history mutation
    // the run loop makes is mirrored here.
    let journal =
        state_persistence::history::HistoryJournal::new(dir.path().join("s-1.history.jsonl"));
    let mut conversation = Conversation::with_sink(Arc::new(
        crate::history_journal::JournalSink::new(journal.clone(), 0),
    ));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen_events = seen.clone();
    let outcome = RunLoop::new(
        tools,
        policy,
        hooks,
        adapter,
        HeadlessDeny,
        plans,
        compactor,
        runtime_runner::RunConfig {
            model_name: "scripted".to_string(),
            context_window: 200_000,
            // nosemgrep: codacy.yaml.security.hard-coded-tokens
            max_output_tokens: 100,
            max_tool_rounds: 8,
            max_continuations: runtime_runner::MAX_CONTINUATIONS,
            max_plan_nudges: runtime_runner::MAX_PLAN_NUDGES,
            max_goal_continuations: runtime_runner::MAX_GOAL_CONTINUATIONS,
            max_goal_rearms: runtime_runner::MAX_GOAL_REARMS,
            max_stop_blocks: runtime_runner::MAX_STOP_BLOCKS,
            max_reactive_compacts: runtime_runner::MAX_REACTIVE_COMPACTS,
            max_repeat_streak: runtime_runner::MAX_REPEAT_STREAK,
            max_wire_images: runtime_runner::MAX_WIRE_IMAGES,
        },
        interrupt,
    )
    .run_turn(
        &runtime_runner::RunContext {
            run_id: "proof".to_string(),
            submission_id: "proof".to_string(),
            input: "write and report".to_string(),
            images: Vec::new(),
        },
        &mut conversation,
        runtime_runner::TurnInput::text("write and report"),
        "sys",
        &|event| {
            seen_events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(event);
        },
    )
    .await;
    assert_eq!(outcome, runtime_runner::StopReason::Completed);
    // The write really landed on disk with exact content.
    assert_eq!(
        std::fs::read_to_string(dir.path().join("hello.txt")).unwrap(),
        "wavecode-smoke-ok"
    );
    // Both rounds ran: two assistant messages, one paired tool call.
    let kinds: Vec<String> = seen
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|event| {
            serde_json::to_value(&event.msg)
                .unwrap()
                .get("type")
                .unwrap()
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(
        kinds
            .iter()
            .filter(|k| *k == "agent_message_complete")
            .count(),
        2
    );
    assert_eq!(kinds.iter().filter(|k| *k == "tool_call_begin").count(), 1);
    assert_eq!(kinds.iter().filter(|k| *k == "tool_call_end").count(), 1);

    // Every entry the run appended is on disk, and the tool call
    // survives as a block pair rather than as flattened prose.
    let replayed = crate::history_journal::replay_history(&journal);
    assert_eq!(
        replayed.entries.len(),
        conversation.len(),
        "journal fell behind the conversation"
    );
    assert!(replayed.lost_calls.is_empty(), "{:?}", replayed.lost_calls);
    assert!(
        !replayed.gapped && !replayed.torn_tail,
        "clean run reported damage"
    );
    let calls: Vec<&str> = replayed
        .entries
        .iter()
        .flat_map(|entry| entry.blocks.iter())
        .filter_map(|block| match block {
            state_store::Block::ToolUse { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(calls, ["write"]);
    assert!(
        replayed
            .entries
            .iter()
            .flat_map(|entry| entry.blocks.iter())
            .any(|block| matches!(
                block,
                state_store::Block::ToolResult {
                    is_error: false,
                    ..
                }
            )),
        "the tool result never reached the journal"
    );

    // Resuming from that journal yields the same history, and doing it
    // twice must not grow it: the journal, not the caller, is the source.
    let first = seed_conversation(Some(journal.clone()), &[], &mut Vec::new());
    let second = seed_conversation(Some(journal), &[], &mut Vec::new());
    assert_eq!(first.len(), conversation.len());
    assert_eq!(second.len(), first.len());
}

/// A sequence hole must not hide records appended after resume.
#[test]
fn gapped_journal_is_rewritten_before_new_records() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.jsonl");
    let journal = state_persistence::history::HistoryJournal::new(&path);
    use state_persistence::history::HistoryRecord;
    journal
        .append_record(&HistoryRecord::Append {
            seq: 0,
            entry: serde_json::json!({"role": "user", "blocks": [{"text": "kept"}]}),
        })
        .unwrap();
    journal
        .append_record(&HistoryRecord::Append {
            seq: 2,
            entry: serde_json::json!({"role": "assistant", "blocks": [{"text": "dropped"}]}),
        })
        .unwrap();
    let mut warnings = Vec::new();
    let mut conv = seed_conversation(Some(journal.clone()), &[], &mut warnings);
    assert!(
        warnings.iter().any(|warning| warning.contains("prefix")),
        "{warnings:?}"
    );
    assert_eq!(conv.snapshot()[0].text(), "kept");
    // The prefix is durable before any later append: one replace record,
    // and the record that sat past the hole is gone.
    let repaired = crate::history_journal::replay_history(&journal);
    assert!(!repaired.gapped && !repaired.torn_tail, "{repaired:?}");
    assert_eq!(repaired.entries.len(), 1);
    assert_eq!(repaired.entries[0].text(), "kept");
    assert_eq!(journal.read().records.len(), 1);
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(!raw.contains("dropped"), "{raw}");
    conv.push(state_store::Role::Assistant, "after");
    let replayed = crate::history_journal::replay_history(&journal);
    assert!(!replayed.gapped && !replayed.torn_tail, "{replayed:?}");
    assert_eq!(replayed.entries.len(), 2);
    assert_eq!(replayed.entries[1].text(), "after");
}
