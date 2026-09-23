/*!
 * @file OfflineBenchmarks
 * @description Offline benches pinning turn performance and shape.
 *
 * Responsibilities:
 * - Drive scripted models through the real run loop and time it.
 * - Report tokens and rounds as JSON for baseline comparison.
 * - Gate live-provider smoke behind LIVE=1 without ever failing.
 *
 * This module must not depend on: network, credentials, or interpreters.
 * Every test runs keyless; timing fails only beyond 10x baseline median.
 */

use infrastructure_base::InterruptHandle;
use runtime_runner::{
    ApprovalResolution, ApprovalSource, AskKind, CompactError, Compacted, Compactor, HookGateway,
    HookPoint, HookReport, ModelGateway, PlanTracker, PolicyDecider, PolicyVerdict, RunConfig,
    RunContext, RunLoop, SampleBlock, SampleError, SampleRequest, SampleResponse, StopReason,
    ToolCall, ToolExecutor, ToolRef, ToolResult,
};
use state_store::{CompactTrigger, Conversation, HistoryEntry};
use wavecode_wire::Event;

// Keep in sync with benchmarks/run.rs and benchmarks/baseline.json.
const TOOL_ROUNDS: usize = 8;
const SESSION_TURNS: usize = 5;
const SESSION_ROUNDS_PER_TURN: usize = 2;
const CONTINUATION_BOUND_MS: u128 = 10_000;
const SESSION_BOUND_MS: u128 = 15_000;
const BASELINE_JSON: &str = include_str!("../../../../benchmarks/baseline.json");

/// Echo executor: tools succeed instantly with no side effects.
struct EchoExecutor;

#[async_trait::async_trait]
impl ToolExecutor for EchoExecutor {
    async fn execute(&self, call: ToolCall) -> ToolResult {
        ToolResult {
            call_id: call.call_id.clone(),
            content: format!("ok:{}", call.input),
            is_error: false,
        }
    }

    fn available_tools(&self) -> Vec<ToolRef> {
        vec![ToolRef {
            name: "echo".to_string(),
            description: "bench echo".to_string(),
        }]
    }
}

/// Permissive policy: every call runs without approval gates.
struct AllowPolicy;

#[async_trait::async_trait]
impl PolicyDecider for AllowPolicy {
    async fn decide(&self, _call: &ToolCall) -> PolicyVerdict {
        PolicyVerdict::Allow
    }
}

/// Silent hooks: nothing blocks, nothing warns.
struct NullHooks;

#[async_trait::async_trait]
impl HookGateway for NullHooks {
    async fn run(&self, _point: HookPoint, _payload: &str) -> HookReport {
        HookReport {
            allow: true,
            message: String::new(),
            context: String::new(),
        }
    }
}

/// Scripted model: emits one tool call per sample for `remaining` samples,
/// then a final text answer. No network, fully deterministic.
struct ScriptedToolModel {
    remaining: std::sync::Mutex<usize>,
}

impl ScriptedToolModel {
    fn with_rounds(rounds: usize) -> Self {
        Self {
            remaining: std::sync::Mutex::new(rounds),
        }
    }
}

#[async_trait::async_trait]
impl ModelGateway for ScriptedToolModel {
    async fn sample(&self, _request: SampleRequest) -> Result<SampleResponse, SampleError> {
        let mut guard = self.remaining.lock().unwrap_or_else(|e| e.into_inner());
        if *guard > 0 {
            *guard -= 1;
            let call_id = format!("bench-c{guard}");
            return Ok(SampleResponse {
                blocks: vec![SampleBlock::ToolUse {
                    call_id,
                    name: "echo".to_string(),
                    input: serde_json::json!({"n": *guard}),
                }],
                input_tokens: Some(10),
                output_tokens: Some(5),
                truncated: false,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            });
        }
        Ok(SampleResponse {
            blocks: vec![SampleBlock::Text("bench-done".to_string())],
            input_tokens: Some(10),
            output_tokens: Some(5),
            truncated: false,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        })
    }
}

/// Denying approvals: the benches never park on a gate nobody answers.
struct DenyApproval;

#[async_trait::async_trait]
impl ApprovalSource for DenyApproval {
    async fn decide(&self, _call_id: &str, _kind: AskKind, _detail: &str) -> ApprovalResolution {
        ApprovalResolution::Deny {
            reason: "bench denies approvals".to_string(),
        }
    }

    fn clear_stale(&self) {}
}

/// Empty plan tracker: no steering reminders during benches.
struct EmptyPlan;

impl PlanTracker for EmptyPlan {
    fn unfinished(&self) -> usize {
        0
    }

    fn reminder(&self) -> String {
        String::new()
    }
}

/// Null compactor: the wide context window means this never runs.
struct NullCompactor;

#[async_trait::async_trait]
impl Compactor for NullCompactor {
    async fn compact(
        &self,
        _history: Vec<HistoryEntry>,
        _trigger: CompactTrigger,
    ) -> Result<Compacted, CompactError> {
        Ok(Compacted {
            summary: "bench".to_string(),
            summary_tokens: 1,
        })
    }
}

type BenchLoop = RunLoop<
    EchoExecutor,
    AllowPolicy,
    NullHooks,
    ScriptedToolModel,
    DenyApproval,
    EmptyPlan,
    NullCompactor,
>;

fn bench_config() -> RunConfig {
    RunConfig {
        model_name: "bench-scripted".to_string(),
        context_window: 200_000,
        max_output_tokens: 100,
        max_tool_rounds: 32,
        max_continuations: runtime_runner::MAX_CONTINUATIONS,
        max_plan_nudges: runtime_runner::MAX_PLAN_NUDGES,
        max_goal_continuations: runtime_runner::MAX_GOAL_CONTINUATIONS,
        max_goal_rearms: runtime_runner::MAX_GOAL_REARMS,
        max_stop_blocks: runtime_runner::MAX_STOP_BLOCKS,
        max_repeat_streak: 0,
        max_wire_images: 0,
        session_date: None,
        max_reactive_compacts: runtime_runner::MAX_REACTIVE_COMPACTS,
    }
}

fn make_loop(rounds: usize) -> BenchLoop {
    RunLoop::new(
        EchoExecutor,
        AllowPolicy,
        NullHooks,
        ScriptedToolModel::with_rounds(rounds),
        DenyApproval,
        EmptyPlan,
        NullCompactor,
        bench_config(),
        InterruptHandle::new(),
    )
}

/// Wire event type tags in emission order.
fn event_types(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .map(|event| {
            serde_json::to_value(&event.msg)
                .expect("event serializes")
                .get("type")
                .expect("event has type tag")
                .as_str()
                .expect("type tag is a string")
                .to_string()
        })
        .collect()
}

/// Sum of reported output tokens across TokenCount events.
fn summed_output_tokens(events: &[Event]) -> u64 {
    events
        .iter()
        .filter_map(|event| {
            let value = serde_json::to_value(&event.msg).ok()?;
            if value.get("type")?.as_str()? != "token_count" {
                return None;
            }
            value.get("output_tokens")?.as_u64()
        })
        .sum()
}

fn baseline_numbers(bench: &str) -> (f64, f64, f64) {
    let baseline: serde_json::Value =
        serde_json::from_str(BASELINE_JSON).expect("baseline.json parses");
    let median = baseline
        .pointer(&format!("/benches/{bench}/median_ms"))
        .and_then(|v| v.as_f64())
        .unwrap_or_else(|| panic!("baseline median for {bench} present"));
    let warn = baseline
        .get("warn_beyond")
        .and_then(|v| v.as_f64())
        .expect("warn_beyond present");
    let fail = baseline
        .get("fail_beyond")
        .and_then(|v| v.as_f64())
        .expect("fail_beyond present");
    (median, warn, fail)
}

/// PASS/WARN print, FAIL panics. Correctness is asserted separately and
/// always fails; timing only fails beyond the 10x fail band.
fn check_timing(bench: &str, detail: &str, elapsed_ms: u128) {
    let (median, warn, fail) = baseline_numbers(bench);
    let elapsed = elapsed_ms as f64;
    let verdict = if elapsed <= median * warn {
        "PASS"
    } else if elapsed <= median * fail {
        "WARN"
    } else {
        "FAIL"
    };
    println!(
        "{{\"bench\":\"{bench}\",{detail},\"elapsed_ms\":{elapsed_ms},\"median_ms\":{median},\"verdict\":\"{verdict}\"}}"
    );
    assert_ne!(
        verdict, "FAIL",
        "{bench} slower than {fail}x baseline median ({median}ms)"
    );
}

#[tokio::test]
async fn continuation_micro_bench() {
    let driver = make_loop(TOOL_ROUNDS);
    let mut conv = Conversation::new();
    let seen = std::sync::Mutex::new(Vec::new());
    let start = std::time::Instant::now();
    let outcome = driver
        .run_turn(
            &RunContext {
                run_id: "bench-cont".to_string(),
                submission_id: "bench-cont".to_string(),
                input: "bench input".to_string(),
                images: Vec::new(),
            },
            &mut conv,
            runtime_runner::TurnInput::text("bench input"),
            "bench system",
            &|event| {
                seen.lock().unwrap_or_else(|e| e.into_inner()).push(event);
            },
        )
        .await;
    let elapsed_ms = start.elapsed().as_millis();
    // Correctness first: the product shape must hold before timing matters.
    assert_eq!(outcome, StopReason::Completed);
    let events = seen.lock().unwrap_or_else(|e| e.into_inner());
    let kinds = event_types(&events);
    assert_eq!(
        kinds.iter().filter(|k| *k == "tool_call_begin").count(),
        TOOL_ROUNDS
    );
    assert_eq!(
        kinds.iter().filter(|k| *k == "tool_call_end").count(),
        TOOL_ROUNDS
    );
    assert_eq!(
        kinds
            .iter()
            .filter(|k| *k == "agent_message_complete")
            .count(),
        TOOL_ROUNDS + 1
    );
    assert_eq!(kinds.last().map(String::as_str), Some("turn_completed"));
    let history = conv
        .snapshot()
        .iter()
        .map(|entry| entry.text())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        history.contains("bench-done"),
        "final answer lands in history"
    );
    let tokens = summed_output_tokens(&events);
    assert!(tokens > 0, "token usage is reported");
    assert!(
        elapsed_ms < CONTINUATION_BOUND_MS,
        "continuation stalled: {elapsed_ms}ms"
    );
    check_timing(
        "continuation_micro",
        &format!("\"tool_rounds\":{TOOL_ROUNDS},\"tokens\":{tokens}"),
        elapsed_ms,
    );
}

#[tokio::test]
async fn session_open_bench() {
    // Offline assembly plus several scripted turns on one conversation.
    // Full provider assembly needs credentials, so this pins the offline
    // shape: loop construction, conversation setup, and driven turns.
    let start = std::time::Instant::now();
    let mut conv = Conversation::new();
    let mut total_begins = 0;
    for index in 0..SESSION_TURNS {
        let driver = make_loop(SESSION_ROUNDS_PER_TURN);
        let input = format!("open {index}");
        let seen = std::sync::Mutex::new(Vec::new());
        let outcome = driver
            .run_turn(
                &RunContext {
                    run_id: format!("bench-open-{index}"),
                    submission_id: format!("bench-open-{index}"),
                    input: input.clone(),
                    images: Vec::new(),
                },
                &mut conv,
                runtime_runner::TurnInput::text(&input),
                "bench system",
                &|event| {
                    seen.lock().unwrap_or_else(|e| e.into_inner()).push(event);
                },
            )
            .await;
        assert_eq!(outcome, StopReason::Completed, "turn {index} completes");
        let events = seen.lock().unwrap_or_else(|e| e.into_inner());
        total_begins += event_types(&events)
            .iter()
            .filter(|k| *k == "tool_call_begin")
            .count();
    }
    let elapsed_ms = start.elapsed().as_millis();
    assert_eq!(total_begins, SESSION_TURNS * SESSION_ROUNDS_PER_TURN);
    let history = conv
        .snapshot()
        .iter()
        .map(|entry| entry.text())
        .collect::<Vec<_>>()
        .join("\n");
    for index in 0..SESSION_TURNS {
        assert!(history.contains(&format!("open {index}")));
    }
    assert!(
        elapsed_ms < SESSION_BOUND_MS,
        "session-open stalled: {elapsed_ms}ms"
    );
    check_timing(
        "session_open",
        &format!("\"turns\":{SESSION_TURNS},\"tool_rounds\":{total_begins}"),
        elapsed_ms,
    );
}

/// The harness reads its own baseline: assert it parses and the policy
/// bands are sane so a bad edit fails here, not as a flaky bench.
#[test]
fn baseline_meta_test() {
    let baseline: serde_json::Value =
        serde_json::from_str(BASELINE_JSON).expect("baseline.json parses");
    assert_eq!(baseline.get("version").and_then(|v| v.as_u64()), Some(1));
    let warn = baseline
        .get("warn_beyond")
        .and_then(|v| v.as_f64())
        .expect("warn_beyond is a number");
    let fail = baseline
        .get("fail_beyond")
        .and_then(|v| v.as_f64())
        .expect("fail_beyond is a number");
    assert!(warn >= 1.0, "warn band must cover the median");
    assert!(fail > warn, "fail band must sit above the warn band");
    for bench in ["continuation_micro", "session_open"] {
        let median = baseline
            .pointer(&format!("/benches/{bench}/median_ms"))
            .and_then(|v| v.as_f64())
            .unwrap_or_else(|| panic!("{bench} median present"));
        assert!(median > 0.0, "{bench} median is positive");
    }
}

/// Live-provider smoke gate. Offline by default; with LIVE=1 and a key it
/// still runs only a scripted stand-in turn and prints how to wire a real
/// provider. This test never fails for missing keys or missing network.
#[tokio::test]
async fn live_gate_smoke() {
    let live = std::env::var("LIVE").ok();
    let key_present = std::env::var("ANTHROPIC_API_KEY")
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    if live.as_deref() != Some("1") {
        println!("SKIP live smoke: set LIVE=1 to opt in (offline default)");
        return;
    }
    if !key_present {
        println!("SKIP live smoke: LIVE=1 but ANTHROPIC_API_KEY is absent");
        return;
    }
    // Key present: run one scripted smoke turn as the shape check. A real
    // provider turn stays manual: construct the session ModelAdapter with
    // the live key and drive one turn, then compare its events against the
    // scripted shape asserted in continuation_micro_bench.
    let driver = make_loop(1);
    let mut conv = Conversation::new();
    let outcome = driver
        .run_turn(
            &RunContext {
                run_id: "bench-live".to_string(),
                submission_id: "bench-live".to_string(),
                input: "live smoke".to_string(),
                images: Vec::new(),
            },
            &mut conv,
            runtime_runner::TurnInput::text("live smoke"),
            "bench system",
            &|_| {},
        )
        .await;
    assert_eq!(outcome, StopReason::Completed);
    println!("LIVE=1 smoke shape holds (scripted stand-in, 1 tool round)");
}
