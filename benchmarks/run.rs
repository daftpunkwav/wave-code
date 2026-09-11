/*!
 * @file BenchmarkHarness
 * @description Documented spec for offline benches and replay goldens.
 *
 * Responsibilities:
 * - Document bench foci, bounds, reports, and baseline policy.
 * - Mirror the executable integration tests in readable form.
 * - Define fixture shapes and live-gate behavior for humans.
 *
 * This module must not depend on: any workspace crate. It is a plain
 * documented harness, not a cargo target; the executable form lives in
 * crates/runtime/runner/tests/benchmarks.rs and
 * crates/operations/replay/tests/snapshot_replay.rs. Keep this file in
 * sync with those tests when changing rounds, bounds, or fixtures.
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
