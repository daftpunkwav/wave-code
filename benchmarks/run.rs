/*!
 * @file BenchmarkHarness
 * @description Documented spec for offline benches, replay goldens, tasks.
 *
 * Responsibilities:
 * - Document bench foci, bounds, reports, and baseline policy.
 * - Mirror the executable integration tests in readable form.
 * - Define fixture shapes and live-gate behavior for humans.
 * - State the task-level suite's judging rules and offline gates.
 *
 * This module must not depend on: any workspace crate. It is a plain
 * documented harness, not a cargo target; the executable form lives in
 * crates/runtime/runner/tests/benchmarks.rs,
 * crates/operations/replay/tests/snapshot_replay.rs, and the `task_eval`
 * module of crates/frontends/harness/src. Keep this file in sync with those
 * tests when changing rounds, bounds, or fixture shapes.
 */

//! Benchmark + snapshot-replay harness spec.
//!
//! The functions below are the human-readable form of the integration
//! tests. They are intentionally written against plain data (counts,
//! milliseconds, JSON text) so the policy reads without cargo context.
//! For the running code, see the test files named in the module docs.

/// Scripted-model bench dimensions. Keep in sync with the integration
/// tests: continuation uses 8 tool rounds, session-open uses 5 turns.
pub const CONTINUATION_TOOL_ROUNDS: u32 = 8;
/// Turns driven by the session-open bench after offline assembly.
pub const SESSION_OPEN_TURNS: u32 = 5;
/// Generous wall-time bounds (ms). Real runs take low tens of ms; these
/// bounds only catch catastrophic stalls, never CI noise.
pub const CONTINUATION_WALL_BOUND_MS: u128 = 10_000;
/// Session-open covers assembly plus several turns, hence the wider bound.
pub const SESSION_OPEN_WALL_BOUND_MS: u128 = 15_000;
/// Replay goldens are pure CPU over tiny inputs; still generous.
pub const REPLAY_GOLDEN_WALL_BOUND_MS: u128 = 5_000;

/// Timing verdict. WARN still passes; only FAIL fails, and only beyond
/// 10x median or on a correctness mismatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// At or under warn threshold.
    Pass,
    /// Above warn threshold, within fail threshold.
    Warn,
    /// Beyond fail threshold: real regression, fail the test.
    Fail,
}

/// Compare one elapsed sample against a committed median.
///
/// `warn_beyond` and `fail_beyond` come from `baseline.json`
/// (2.0 and 10.0). Single samples are noisy by design, so the fail band
/// is an order of magnitude wide.
pub fn compare_against_baseline(
    elapsed_ms: f64,
    median_ms: f64,
    warn_beyond: f64,
    fail_beyond: f64,
) -> Verdict {
    if elapsed_ms <= median_ms * warn_beyond {
        Verdict::Pass
    } else if elapsed_ms <= median_ms * fail_beyond {
        Verdict::Warn
    } else {
        Verdict::Fail
    }
}

/// Shape of the JSON report each bench prints (one line, `--nocapture`).
///
/// Example:
/// `{"bench":"continuation_micro","tool_rounds":8,"elapsed_ms":12,
/// "verdict":"PASS","median_ms":800.0}`.
/// Reports are informational; verdicts come from `compare_against_baseline`
/// plus correctness assertions in the tests.
pub fn report_line(bench: &str, detail: &str, elapsed_ms: u128, verdict: Verdict) -> String {
    let verdict_name = match verdict {
        Verdict::Pass => "PASS",
        Verdict::Warn => "WARN",
        Verdict::Fail => "FAIL",
    };
    format!(
        "{{\"bench\":\"{bench}\",{detail},\"elapsed_ms\":{elapsed_ms},\"verdict\":\"{verdict_name}\"}}"
    )
}

/// Live-gate decision. Everything runs offline unless the caller sets
/// `LIVE=1`; even then a missing provider key skips instead of failing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveDecision {
    /// Run the one smoke turn (live key present).
    Run,
    /// Print SKIP and pass (default path, or key missing).
    Skip,
}

/// Resolve whether the live smoke turn may run. Never errors: unknown or
/// absent inputs always resolve to `Skip`, never to failure.
pub fn live_gate(live_var: Option<&str>, provider_key_present: bool) -> LiveDecision {
    match live_var {
        Some("1") if provider_key_present => LiveDecision::Run,
        _ => LiveDecision::Skip,
    }
}

/// Committed transcript fixtures pinning user-visible replay behavior.
/// Each file is a JSON array of wire events; see `fixtures/*.json`.
pub const FIXTURES: &[&str] = &["basic", "approval", "interrupt", "truncation"];

/// Task-level suite (`tasks/<id>/task.toml` + `workspace/`): the one tier
/// that drives a real model. Lower bound on the committed task count, which
/// `committed_tasks_are_well_formed` asserts against the directory itself —
/// the list lives on disk, so this file only states the floor.
pub const MIN_TASKS: usize = 8;

/// How a task is judged. The agent edits a copy of `workspace/`; then:
///
/// - every assertion must hold, and
/// - the turn must have ended cleanly (exit 0). A crashed, interrupted, or
///   capped turn fails regardless of what the files look like, because a
///   crash leaves no basis for trusting them.
///
/// Assertion kinds: `command` (argv, no shell, expected exit code),
/// `file_equals`, `file_contains`, `file_not_contains`, `file_exists`,
/// `file_absent`, and `file_unchanged` (byte-for-byte against the pre-turn
/// copy — the check that a fix did not trample a neighbour).
///
/// Text comparisons normalize CRLF to LF, because the fixture reaches the
/// work root through a git checkout whose line endings differ per platform.
/// `file_unchanged` does not: "unchanged" means the same bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskVerdict {
    /// Turn clean, every assertion satisfied.
    Pass,
    /// Turn failed, or at least one assertion did not hold.
    Fail,
}

/// The three offline gates that keep the suite worth running at all. They
/// execute inside ordinary `cargo test` and need no credentials:
///
/// 1. well-formed — manifests load, ids are unique and match their
///    directory, and a cargo fixture carries its own `[workspace]` table so
///    the enclosing workspace cannot swallow it;
/// 2. not already solved — the pristine fixture must fail an assertion, or
///    the task hands out a free point;
/// 3. solvable — a known-good end state must satisfy every assertion, so a
///    mistyped expectation is caught here instead of by a live run.
///
/// Gate 2 and 3 skip `command` assertions on purpose: running them would put
/// a real build on the critical path of every CI job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskGate {
    WellFormed,
    NotAlreadySolved,
    Solvable,
}
