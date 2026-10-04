/*!
 * @file CachePrefixStability
 * @description Regression guards for the provider prompt-cache prefix.
 *
 * What these tests pin is the property that makes long sessions cheap: the
 * serialized request must keep extending, not rewriting, so the cached prefix
 * survives turn to turn. A rewrite costs a full-price re-read of everything
 * from the divergence point onward, which is exactly the cost a 500-turn task
 * cannot afford.
 *
 * Measured context (2026-09-21, see docs-local/longhorizon-plan-20260921.md):
 * below the eviction threshold a growing conversation diverges zero times;
 * crossing it rewrites almost the whole history in a single step, after which
 * roughly 4.1k tokens re-read on 62 of the next 60-odd turns.
 */

use state_store::{Block, HistoryEntry, Role, estimate_tokens};
use wavecode_context::{
    DEFAULT_EVICTION_BATCH_MESSAGES, EvictionConfig, evict_old_tool_results,
    should_evict_tool_results,
};
use wavecode_llm::{ContentBlock, Message, Role as LlmRole};

/// One turn of history: prompt, tool call, and a chatty ~1k-token result.
fn turn_entries(round: usize) -> Vec<HistoryEntry> {
    let id = format!("c{round}");
    vec![
        HistoryEntry {
            role: Role::User,
            blocks: vec![Block::Text(format!("do thing {round}"))],
        },
        HistoryEntry {
            role: Role::Assistant,
            blocks: vec![Block::ToolUse {
                call_id: id.clone(),
                name: "grep".to_string(),
                input: serde_json::json!({"pattern": format!("p{round}")}),
            }],
        },
        HistoryEntry {
            role: Role::User,
            blocks: vec![Block::ToolResult {
                call_id: id,
                content: format!(
                    "round {round} output\n{}",
                    "matched line with some contextual text here\n".repeat(90)
                ),
                is_error: false,
                produced_at: None,
            }],
        },
    ]
}

fn to_message(entry: &HistoryEntry) -> Message {
    Message {
        role: if entry.role == Role::Assistant {
            LlmRole::Assistant
        } else {
            LlmRole::User
        },
        content: entry
            .blocks
            .iter()
            .map(|b| match b {
                Block::Text(t) => ContentBlock::Text { text: t.clone() },
                Block::ToolUse {
                    call_id,
                    name,
                    input,
                } => ContentBlock::ToolUse {
                    id: call_id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                },
                Block::ToolResult {
                    call_id,
                    content,
                    is_error,
                    ..
                } => ContentBlock::ToolResult {
                    tool_use_id: call_id.clone(),
                    content: content.clone(),
                    is_error: *is_error,
                },
                other => ContentBlock::Text {
                    text: format!("{other:?}"),
                },
            })
            .collect(),
    }
}

/// First message index where two projections differ: the point past which the
/// provider can no longer reuse its cached prefix.
fn divergence(a: &[Message], b: &[Message]) -> usize {
    let mut i = 0;
    while i < a.len() && i < b.len() && a[i] == b[i] {
        i += 1;
    }
    i
}

/// The request shape the gateway sends: history as-is below the soft
/// threshold, micro-compacted above it.
fn projected(history: &[HistoryEntry], cfg: &EvictionConfig) -> (Vec<Message>, bool) {
    let messages: Vec<Message> = history.iter().map(to_message).collect();
    let tokens = messages.iter().fold(0u64, |sum, m| {
        sum + m.content.iter().fold(0u64, |inner, b| match b {
            ContentBlock::Text { text } | ContentBlock::ToolResult { content: text, .. } => {
                inner + estimate_tokens(text)
            }
            other => inner + (serde_json::to_string(other).unwrap().len() as u64 / 4),
        })
    });
    if should_evict_tool_results(tokens, cfg) {
        (evict_old_tool_results(&messages, cfg), true)
    } else {
        (messages, false)
    }
}

/// Below the eviction threshold a growing session must never rewrite bytes it
/// already sent. A future change that normalizes, truncates, or re-pairs old
/// messages on every request fails here rather than in a bill.
#[test]
fn growing_history_never_rewrites_its_prefix_below_the_eviction_threshold() {
    let cfg = EvictionConfig::default();
    let mut history: Vec<HistoryEntry> = Vec::new();
    let mut previous: Option<Vec<Message>> = None;
    let mut transitions = 0usize;

    for round in 0..60 {
        history.extend(turn_entries(round));
        let (request, evicted) = projected(&history, &cfg);
        assert!(!evicted, "probe history must stay under the threshold");
        if let Some(previous) = &previous {
            transitions += 1;
            assert_eq!(
                divergence(previous, &request),
                previous.len(),
                "turn {round} rewrote a message the provider had already cached"
            );
        }
        previous = Some(request);
    }
    assert_eq!(transitions, 59);
}

/// Eviction must be idempotent: re-projecting an already-projected history
/// changes no bytes, so a stub never gets rewritten into a different stub
/// (which would break the cache on an otherwise stable turn).
#[test]
fn eviction_pass_is_idempotent() {
    let cfg = EvictionConfig {
        // nosemgrep: Semgrep_codacy.yaml.security.hard-coded-tokens
        soft_threshold_tokens: 0,
        ..EvictionConfig::default()
    };
    let history: Vec<HistoryEntry> = (0..40).flat_map(turn_entries).collect();
    let raw: Vec<Message> = history.iter().map(to_message).collect();
    let once = evict_old_tool_results(&raw, &cfg);
    let twice = evict_old_tool_results(&once, &cfg);
    assert_ne!(
        once, raw,
        "the pass evicted nothing: idempotency is vacuous"
    );
    assert_eq!(once, twice, "a second eviction pass changed bytes");
}

/// The advertised catalog is part of the cacheable prefix, so its order must be
/// a pure function of the tool set. Name-sorted is the contract: a hash-order
/// or registration-order leak would reshuffle every request.
#[test]
fn advertised_catalog_order_is_a_function_of_its_names() {
    let (registry, _todos) = wavecode_tools::Registry::builtin_with_todos();
    let first: Vec<String> = registry.specs().iter().map(|s| s.name.clone()).collect();
    let second: Vec<String> = registry.specs().iter().map(|s| s.name.clone()).collect();
    assert_eq!(first, second, "catalog order must be deterministic");
    let mut sorted = first.clone();
    sorted.sort();
    assert_eq!(first, sorted, "catalog must stay name-sorted");
}

/// Quantified reason for batching. With the same history growth, a
/// per-message frontier rewrites the request on nearly every turn, while the
/// default batched frontier rewrites once per batch. Each rewrite costs a
/// full-price re-read of everything behind it, so the count is the cost.
#[test]
fn batched_frontier_diverges_far_less_often_than_per_message() {
    let breaks_for = |batch_messages: usize| -> usize {
        let cfg = EvictionConfig {
            batch_messages,
            ..EvictionConfig::default()
        };
        let mut history: Vec<HistoryEntry> = Vec::new();
        let mut previous: Option<Vec<Message>> = None;
        let mut breaks = 0usize;
        for round in 0..160 {
            history.extend(turn_entries(round));
            let (request, _) = projected(&history, &cfg);
            if let Some(previous) = &previous
                && divergence(previous, &request) < previous.len()
            {
                breaks += 1;
            }
            previous = Some(request);
        }
        breaks
    };

    let per_message = breaks_for(1);
    let batched = breaks_for(DEFAULT_EVICTION_BATCH_MESSAGES);
    assert!(
        per_message > 20,
        "the fixture must actually exercise eviction: per-message broke {per_message} times"
    );
    assert!(
        batched * 3 < per_message,
        "batching saved nothing: batched={batched} per_message={per_message}"
    );
}
