//! wavecode-hooks — lifecycle hooks system (SPEC section 9; P7 lands the command type).
//!
//! Event points: PreToolUse / PostToolUse / UserPromptSubmit / SessionStart /
//! SessionEnd / Stop / PreCompact / PostCompact (the SPEC section 9 table; the
//! Notification point is not implemented in this version, left as a placeholder).
//!
//! Hook types:
//! - `command` (implemented in this version): a `[hooks.<EventPoint>]` table
//!   (or table array) config with matcher / command / timeout_ms / once
//!   fields, executed via the platform shell (Windows `cmd /C`, Unix `sh -c`,
//!   overridable with `WAVECODE_SHELL` — the same heuristic as the shell
//!   tool), with the event payload written to stdin as JSON;
//! - `prompt` (SPEC schedules it post-M4): asks a model to rule allow/block
//!   from a template; **not implemented** in this version, left as a
//!   placeholder (see the [`HookDef`] notes).
//!
//! Blocking semantics (SPEC section 9): exit code 0 allows; 2 blocks with
//! stderr fed back to the model (only on blockable points: PreToolUse /
//! UserPromptSubmit / Stop; exit code 2 on other points degrades to an
//! allow-with-warning); any other nonzero code allows with a warning; timeouts
//! force-kill and log a warning.
//!
//! Trust boundary (unlike the shell tool): hook commands come from the user's
//! own config file, so they are authorized configuration rather than
//! model-produced output — no env-var stripping / path constraints apply.
//! Config diagnostics: [`HookEngine::validate`] statically flags entries that
//! can never take effect (empty commands, blank matchers, matchers on
//! non-tool points); the assembly layer should surface them once at startup.
//! [`HookDef::effective_timeout`] normalizes `timeout_ms == 0` to
//! [`DEFAULT_TIMEOUT_MS`] ("unset", not "zero wait").

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

/// Unified lock-poisoning recovery policy (single decision point for this
/// crate): the once-fired set's critical section is a single insert, so a
/// panic while holding the lock leaves no half-written invariant — recover the
/// guard from the poison and carry on (the panic already propagated on its own
/// thread), never cascade into a second panic.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Default timeout (SPEC section 9 config example): 10s.
pub const DEFAULT_TIMEOUT_MS: u64 = 10_000;

/// Event point (SPEC section 9 table; the config `[hooks.<EventPoint>]` table
/// names match the legal values of [`HookEventPoint::parse`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookEventPoint {
    /// Before tool execution (blockable): command audit, pre-flight checks.
    PreToolUse,
    /// After tool execution (not blockable): lint / format write-backs,
    /// notifications.
    PostToolUse,
    /// Before user input enters the turn (blockable): inject extra context,
    /// sensitive-word interception.
    UserPromptSubmit,
    /// Session start (not blockable): environment setup.
    SessionStart,
    /// Session end (not blockable): cleanup.
    SessionEnd,
    /// Before the turn winds down (blockable): refuse to finish while goal
    /// mode is unsatisfied.
    Stop,
    /// Before compaction (not blockable): archival.
    PreCompact,
    /// After compaction (not blockable): archival.
    PostCompact,
    // TODO(SPEC section 9, off-table): the Notification point (system
    // notification forwarding) is not implemented in this version.
}

impl HookEventPoint {
    /// All event points (for docs / validation, fixed order).
    pub const ALL: [HookEventPoint; 8] = [
        Self::PreToolUse,
        Self::PostToolUse,
        Self::UserPromptSubmit,
        Self::SessionStart,
        Self::SessionEnd,
        Self::Stop,
        Self::PreCompact,
        Self::PostCompact,
    ];

    /// Parse an event point name (a config `[hooks.<EventPoint>]` table name).
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str() == name)
    }

    /// Event point name (shared by config table names / payload and
    /// diagnostics text).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::SessionStart => "SessionStart",
            Self::SessionEnd => "SessionEnd",
            Self::Stop => "Stop",
            Self::PreCompact => "PreCompact",
            Self::PostCompact => "PostCompact",
        }
    }

    /// Whether blockable (SPEC section 9 table): exit code 2 on a blockable
    /// point blocks and feeds back stderr; on a non-blockable point it
    /// degrades to an allow-with-warning.
    pub fn blockable(self) -> bool {
        matches!(self, Self::PreToolUse | Self::UserPromptSubmit | Self::Stop)
    }
}

/// One hook definition (one row of a `[hooks.<EventPoint>]` table).
///
/// First version is command-type only: this struct is the command hook; when
/// the prompt type lands (SPEC section 9, model ruling from a template, post
/// M4) this is expected to become a tagged enum (`type = "command" |
/// "prompt"`), with the config surface evolving in step.
#[derive(Debug, Clone)]
pub struct HookDef {
    /// Tool-name matcher (only meaningful on tool-ish points): `|`-separated
    /// alternatives, `*` matches everything; None = no filtering. Entries
    /// with a matcher on non-tool points never fire.
    pub matcher: Option<String>,
    /// Shell command string (executed via the platform shell).
    pub command: String,
    /// Timeout in milliseconds; a timeout force-kills and logs a warning.
    pub timeout_ms: u64,
    /// Fire at most once per session (firing means "actually executed" — a
    /// matcher miss does not consume the quota).
    pub once: bool,
}

impl HookDef {
    /// Effective timeout for execution: `timeout_ms == 0` means "unset"
    /// and falls back to [`DEFAULT_TIMEOUT_MS`]. A zero
    /// `tokio::time::timeout` could never win the race against spawn +
    /// pipe setup, so treating 0 as "no waiting" was a trap (every
    /// execution reported Timeout and consumed `once`); 0 now behaves
    /// like an omitted value.
    pub fn effective_timeout(&self) -> Duration {
        Duration::from_millis(if self.timeout_ms == 0 {
            DEFAULT_TIMEOUT_MS
        } else {
            self.timeout_ms
        })
    }
}

/// An event payload for one hook firing.
#[derive(Debug, Clone, Copy)]
pub struct HookInput<'a> {
    /// Working directory of the hook process (the session cwd).
    pub cwd: &'a Path,
    /// Tool name (PreToolUse / PostToolUse; None on all other points).
    pub tool_name: Option<&'a str>,
    /// Tool input (PreToolUse / PostToolUse).
    pub tool_input: Option<&'a serde_json::Value>,
    /// Tool output summary (PostToolUse; None elsewhere).
    pub tool_output: Option<&'a str>,
}

/// A hook run's ruling.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum HookVerdict {
    /// Allow (incl. no hooks / all succeeded / warnings only).
    #[default]
    Allow,
    /// Block: payload is the model-facing stderr (only blockable points
    /// produce this).
    Block(String),
}

/// An execution report for one event point: the ruling plus warnings (nonzero
/// exit codes / timeouts / spawn failures).
#[derive(Debug, Clone, Default)]
pub struct HookReport {
    /// Ruling (Allow / Block).
    pub verdict: HookVerdict,
    /// Warning list (callers turn these into Warning events / log lines).
    pub warnings: Vec<String>,
}

/// Hook engine: the config tables plus the once-fired record.
///
/// Per-session "once" is scoped to the engine instance — a session assembles
/// one engine (SessionConfig holds an `Arc<HookEngine>`), and rebuilding
/// across sessions resets it.
pub struct HookEngine {
    defs: HashMap<HookEventPoint, Vec<HookDef>>,
    /// Fired once-entries: (point, entry index).
    fired: Mutex<HashSet<(HookEventPoint, usize)>>,
    /// Runtime plugin middleware per event point (registration order is run
    /// order; empty unless `register_plugin_hook` was called).
    plugin_hooks: Mutex<HashMap<HookEventPoint, Vec<PluginHook>>>,
}

impl HookEngine {
    /// Build from point -> entry tables.
    pub fn new(defs: HashMap<HookEventPoint, Vec<HookDef>>) -> Self {
        Self {
            defs,
            fired: Mutex::new(HashSet::new()),
            plugin_hooks: Mutex::new(HashMap::new()),
        }
    }

    /// Empty engine (no entries on any point) — the assembly layer skips
    /// wiring when this holds.
    pub fn is_empty(&self) -> bool {
        self.defs.values().all(Vec::is_empty)
    }

    /// Whether a point has entries (points without entries short-circuit
    /// without building a payload).
    pub fn has_hooks(&self, point: HookEventPoint) -> bool {
        self.defs.get(&point).is_some_and(|d| !d.is_empty())
    }

    /// Static configuration diagnostics (no execution): flags entries
    /// that can never do anything useful — empty commands, blank
    /// matchers, matchers on points that carry no tool name, and zero
    /// timeouts (which fall back to [`DEFAULT_TIMEOUT_MS`] at runtime,
    /// see [`HookDef::effective_timeout`]). Assembly layers should
    /// surface these once at startup; [`HookEngine::run`] additionally
    /// warns at runtime for empty commands on the hot path.
    pub fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut points: Vec<HookEventPoint> = self.defs.keys().copied().collect();
        points.sort_by_key(|p| p.as_str()); // deterministic order
        for point in points {
            let Some(defs) = self.defs.get(&point) else {
                continue;
            };
            for (idx, def) in defs.iter().enumerate() {
                if def.command.trim().is_empty() {
                    out.push(format!(
                        "[{}] entry #{idx} has an empty command (never executes)",
                        point.as_str()
                    ));
                }
                if def.timeout_ms == 0 {
                    out.push(format!(
                        "[{}] entry #{idx} timeout_ms is 0 (falls back to {DEFAULT_TIMEOUT_MS}ms)",
                        point.as_str()
                    ));
                }
                if let Some(matcher) = &def.matcher {
                    if matcher.trim().is_empty() {
                        out.push(format!(
                            "[{}] entry #{idx} matcher is blank (never matches)",
                            point.as_str()
                        ));
                    } else if !is_tool_point(point) {
                        out.push(format!(
                            "[{}] entry #{idx} has a matcher but this point carries no tool name (never fires)",
                            point.as_str()
                        ));
                    }
                }
            }
        }
        out
    }

    /// Fire one event point: execute entries in config order, collecting
    /// warnings; the first exit code 2 on a blockable point short-circuits to
    /// Block (later entries no longer run — a block is final).
    pub async fn run(&self, point: HookEventPoint, input: &HookInput<'_>) -> HookReport {
        let mut report = HookReport::default();
        let Some(defs) = self.defs.get(&point) else {
            return report;
        };
        for (idx, def) in defs.iter().enumerate() {
            // Matcher filtering: only tool-ish points can match; an entry
            // with a matcher on a non-tool point never fires (surfaced at
            // startup via [`HookEngine::validate`]).
            if let Some(matcher) = &def.matcher {
                match input.tool_name {
                    Some(tool) if matcher_matches(matcher, tool) => {}
                    _ => continue,
                }
            }
            // Empty command: config error, never spawns. Warn and skip
            // *before* consuming `once` so a fixed config can still fire
            // this session (same "no execution, no consumption" rule as
            // matcher misses and spawn failures).
            if def.command.trim().is_empty() {
                report.warnings.push(format!(
                    "[{}] entry #{idx} has an empty command (skipped)",
                    point.as_str()
                ));
                continue;
            }
            // once: only an "actual execution" past the matcher consumes quota.
            if def.once && !lock(&self.fired).insert((point, idx)) {
                continue;
            }
            match run_command(point, def, input).await {
                ExecOutcome::Success => {}
                ExecOutcome::Blocked(stderr) => {
                    if point.blockable() {
                        report.verdict = HookVerdict::Block(stderr);
                        return report; // First block short-circuits (a block is final).
                    }
                    report.warnings.push(format!(
                        "[{}] hook `{}` exited with code 2, but this point is not blockable (allowed with warning)",
                        point.as_str(),
                        def.command
                    ));
                }
                ExecOutcome::NonZero(code, stderr) => {
                    report.warnings.push(format!(
                        "[{}] hook `{}` exited with code {code} (allowed with warning): {}",
                        point.as_str(),
                        def.command,
                        one_line(&stderr)
                    ));
                }
                ExecOutcome::Timeout => {
                    report.warnings.push(format!(
                        "[{}] hook `{}` timed out ({}ms), force-killed (allowed with warning)",
                        point.as_str(),
                        def.command,
                        def.effective_timeout().as_millis()
                    ));
                }
                ExecOutcome::SpawnFailed(reason) => {
                    // A process that never started means the hook never ran:
                    // refund the once quota so the entry can still fire later
                    // this session once the config is fixed (a timeout counts
                    // as executed — the process really ran — so its quota
                    // stays consumed).
                    if def.once {
                        lock(&self.fired).remove(&(point, idx));
                    }
                    report.warnings.push(format!(
                        "[{}] hook `{}` failed to spawn (allowed with warning): {reason}",
                        point.as_str(),
                        def.command
                    ));
                }
            }
        }
        report
    }
}

/// Points that carry a tool name in [`HookInput::tool_name`]
/// (PreToolUse / PostToolUse; all other points pass None, so a matcher
/// configured on them can never fire).
fn is_tool_point(point: HookEventPoint) -> bool {
    matches!(
        point,
        HookEventPoint::PreToolUse | HookEventPoint::PostToolUse
    )
}

/// Matcher matching: `|`-separated alternatives (any hit), `*` matches
/// everything, anything else compares exactly.
fn matcher_matches(matcher: &str, tool: &str) -> bool {
    matcher
        .split('|')
        .map(str::trim)
        .any(|alt| alt == "*" || alt == tool)
}

/// One execution's result (internal).
enum ExecOutcome {
    /// Exit code 0.
    Success,
    /// Exit code 2: payload is the stderr (the model-facing block reason).
    Blocked(String),
    /// Other nonzero exit codes: payload is a stderr excerpt.
    NonZero(i32, String),
    /// Timed out and killed.
    Timeout,
    /// Spawn failed (missing command, …).
    SpawnFailed(String),
}

/// Pick the shell program and its "run this command string" flag (the same
/// heuristic as the tools shell tool): Windows `cmd /C`, Unix `sh -c`;
/// `WAVECODE_SHELL` overrides (a value containing `cmd` uses `/C`, otherwise
/// `-c`).
fn shell_invocation() -> (String, &'static str) {
    if let Ok(custom) = std::env::var("WAVECODE_SHELL") {
        if custom.to_lowercase().contains("cmd") {
            return (custom, "/C");
        }
        return (custom, "-c");
    }
    if cfg!(windows) {
        ("cmd".to_owned(), "/C")
    } else {
        ("sh".to_owned(), "-c")
    }
}

/// Run one command hook: platform shell + stdin payload JSON + timeout kill.
///
/// Timeout killing relies on `kill_on_drop`: the timeout branch drops the
/// `wait_with_output` future, which drops the child, and tokio sends the kill;
/// the known grandchild-reaping limitation matches the shell tool (process-
/// group-level reaping is M2 work, see the tools/shell_tool.rs notes).
async fn run_command(point: HookEventPoint, def: &HookDef, input: &HookInput<'_>) -> ExecOutcome {
    let payload = serde_json::json!({
        "event": point.as_str(),
        "tool": input.tool_name,
        "input": input.tool_input,
        "output": input.tool_output,
    });
    let (prog, flag) = shell_invocation();
    let mut child = match tokio::process::Command::new(prog)
        .arg(flag)
        .arg(&def.command)
        .current_dir(input.cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return ExecOutcome::SpawnFailed(e.to_string()),
    };
    // Write the payload to stdin, then close it (hooks that read stdin see
    // EOF); a write failure (the command exited early without reading stdin)
    // is not an error — still wait for the exit code.
    // Writing and waiting are both inside the timeout: write_all can block
    // forever when the payload exceeds the pipe buffer and the hook process
    // never reads stdin, so leaving it outside the timeout would void the
    // timeout guarantee (hanging the whole turn). A timeout drops the future,
    // which drops the child, and kill_on_drop sends the kill.
    let waited = tokio::time::timeout(def.effective_timeout(), async {
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            let _ = stdin.write_all(payload.to_string().as_bytes()).await;
            let _ = stdin.shutdown().await;
        }
        child.wait_with_output().await
    })
    .await;
    match waited {
        Err(_) => ExecOutcome::Timeout, // future dropped -> kill_on_drop kills the process
        Ok(Err(e)) => ExecOutcome::SpawnFailed(e.to_string()),
        Ok(Ok(output)) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            match output.status.code() {
                Some(0) => ExecOutcome::Success,
                Some(2) => ExecOutcome::Blocked(stderr),
                Some(code) => ExecOutcome::NonZero(code, stderr),
                // Killed by a signal (no exit code): allow with a nonzero warning.
                None => ExecOutcome::NonZero(-1, stderr),
            }
        }
    }
}

/// Single-line a warning text (stderr may be multi-line; take the first line
/// to avoid flooding).
fn one_line(text: &str) -> String {
    text.lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(200)
        .collect()
}

// ---- Runtime plugin middleware (in-session lifecycle) ----
//
// Ordering contract: config hooks run first via [`HookEngine::run`];
// plugin middleware runs after via [`HookEngine::run_plugin_hooks`], in
// registration order. Registering a plugin hook never changes config hook
// behavior: with no plugin hooks registered the chain is the identity.

/// Plugin middleware handler: maps the current event JSON to the next event
/// JSON, or `None` to drop the event (short-circuits the chain).
pub type PluginHookFn =
    std::sync::Arc<dyn Fn(serde_json::Value) -> Option<serde_json::Value> + Send + Sync>;

/// One registered plugin middleware entry (registration order is run order).
pub struct PluginHook {
    /// Plugin name that registered the handler (for diagnostics).
    pub source: String,
    /// Event transform for the chain.
    pub handler: PluginHookFn,
}

impl HookEngine {
    /// Register a plugin middleware handler for one event point.
    ///
    /// Handlers run after config hooks, in registration order. This never
    /// affects [`HookEngine::run`]: config entries execute exactly as
    /// before, with or without plugin handlers present.
    pub fn register_plugin_hook(
        &self,
        source: &str,
        point: HookEventPoint,
        handler: PluginHookFn,
    ) {
        lock(&self.plugin_hooks)
            .entry(point)
            .or_default()
            .push(PluginHook {
                source: source.to_owned(),
                handler,
            });
    }

    /// Number of plugin handlers registered on one point (diagnostics/tests).
    pub fn plugin_hook_count(&self, point: HookEventPoint) -> usize {
        lock(&self.plugin_hooks)
            .get(&point)
            .map(Vec::len)
            .unwrap_or(0)
    }

    /// Run the plugin middleware chain for one point: each handler observes
    /// the current event and returns the next one; `None` short-circuits the
    /// chain to `None` (dropped). With no handlers the event passes through
    /// unchanged (`Some(event)`).
    pub fn run_plugin_hooks(
        &self,
        point: HookEventPoint,
        event: serde_json::Value,
    ) -> Option<serde_json::Value> {
        // Clone the handlers out of the lock so user code never runs under
        // the mutex (a handler registering another handler must not deadlock).
        let handlers: Vec<PluginHookFn> = lock(&self.plugin_hooks)
            .get(&point)
            .map(|entries| entries.iter().map(|entry| entry.handler.clone()).collect())
            .unwrap_or_default();
        let mut current = event;
        for handler in handlers {
            current = handler(current)?;
        }
        Some(current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input<'a>(tool: Option<&'a str>) -> HookInput<'a> {
        HookInput {
            cwd: Path::new("."),
            tool_name: tool,
            tool_input: None,
            tool_output: None,
        }
    }

    fn engine(entries: &[(HookEventPoint, HookDef)]) -> HookEngine {
        let mut defs: HashMap<HookEventPoint, Vec<HookDef>> = HashMap::new();
        for (point, def) in entries {
            defs.entry(*point).or_default().push(def.clone());
        }
        HookEngine::new(defs)
    }

    fn def(command: &str) -> HookDef {
        HookDef {
            matcher: None,
            command: command.to_owned(),
            timeout_ms: DEFAULT_TIMEOUT_MS,
            once: false,
        }
    }

    /// Platform-independent command construction: both cmd and sh understand
    /// `exit N`; stderr output needs per-platform spelling (cmd uses `1>&2`,
    /// sh the same shape with a different joiner).
    fn exit_cmd(code: u32, stderr: &str) -> String {
        if stderr.is_empty() {
            format!("exit {code}")
        } else if cfg!(windows) {
            format!("echo {stderr} 1>&2 & exit {code}")
        } else {
            format!("echo {stderr} 1>&2; exit {code}")
        }
    }

    /// The "sleep" command for timeout tests (cmd has no sleep; ping stands in).
    fn sleep_cmd() -> String {
        if cfg!(windows) {
            "ping -n 10 127.0.0.1 >nul".to_owned()
        } else {
            "sleep 10".to_owned()
        }
    }

    // —— matcher ——

    /// SPEC section 9 acceptance: matcher matching (exact / multi-value /
    /// wildcard / misses skipped).
    #[test]
    fn matcher_semantics() {
        assert!(matcher_matches("shell", "shell"));
        assert!(matcher_matches("shell | write_file", "write_file"));
        assert!(matcher_matches("*", "anything"));
        assert!(!matcher_matches("shell", "write_file"));
        assert!(matcher_matches(" shell ", "shell")); // surrounding spaces in alternatives match after trim
    }

    #[tokio::test]
    async fn matcher_skips_non_matching_tool() {
        let e = engine(&[(
            HookEventPoint::PreToolUse,
            HookDef {
                matcher: Some("shell".to_owned()),
                ..def(&exit_cmd(2, "blocked-stderr"))
            },
        )]);
        // Tool name misses: the hook does not run — no block, no warnings.
        let report = e
            .run(HookEventPoint::PreToolUse, &input(Some("write_file")))
            .await;
        assert_eq!(report.verdict, HookVerdict::Allow);
        assert!(report.warnings.is_empty());
        // Match: blocked.
        let report = e
            .run(HookEventPoint::PreToolUse, &input(Some("shell")))
            .await;
        assert_eq!(
            report.verdict,
            HookVerdict::Block("blocked-stderr".to_owned())
        );
    }

    // —— blocking semantics ——

    /// SPEC section 9 acceptance: exit code 0 allows; 2 blocks with stderr in
    /// the Block payload; other nonzero codes allow with a warning.
    #[tokio::test]
    async fn exit_code_semantics() {
        let ok = engine(&[(HookEventPoint::PreToolUse, def(&exit_cmd(0, "")))]);
        let report = ok
            .run(HookEventPoint::PreToolUse, &input(Some("shell")))
            .await;
        assert_eq!(report.verdict, HookVerdict::Allow);
        assert!(report.warnings.is_empty());

        // stderr assertions use ASCII: on Windows cmd emits non-ASCII bytes in
        // GBK, and UTF-8 lossy decoding replaces them (fidelity of non-ASCII
        // block reasons is limited by the platform code page).
        let blocker = engine(&[(
            HookEventPoint::PreToolUse,
            def(&exit_cmd(2, "no-writes-today")),
        )]);
        let report = blocker
            .run(HookEventPoint::PreToolUse, &input(Some("shell")))
            .await;
        assert_eq!(
            report.verdict,
            HookVerdict::Block("no-writes-today".to_owned())
        );

        let failing = engine(&[(HookEventPoint::PreToolUse, def(&exit_cmd(1, "oops")))]);
        let report = failing
            .run(HookEventPoint::PreToolUse, &input(Some("shell")))
            .await;
        assert_eq!(
            report.verdict,
            HookVerdict::Allow,
            "exit code 1 allows with warning"
        );
        assert_eq!(report.warnings.len(), 1);
        assert!(report.warnings[0].contains("exited with code 1"));
        assert!(report.warnings[0].contains("oops"));
    }

    /// Exit code 2 on a non-blockable point: degrades to allow-with-warning
    /// (the SPEC section 9 table's "blockable" column).
    #[tokio::test]
    async fn exit_2_on_non_blockable_point_degrades_to_warning() {
        let e = engine(&[(HookEventPoint::PostToolUse, def(&exit_cmd(2, "ignored")))]);
        let report = e
            .run(HookEventPoint::PostToolUse, &input(Some("shell")))
            .await;
        assert_eq!(report.verdict, HookVerdict::Allow);
        assert_eq!(report.warnings.len(), 1);
        assert!(report.warnings[0].contains("not blockable"));
    }

    /// Multiple hooks run in order; the first exit code 2 on a blockable
    /// point short-circuits later entries.
    #[tokio::test]
    async fn first_block_short_circuits() {
        let e = engine(&[
            (HookEventPoint::Stop, def(&exit_cmd(2, "first"))),
            (HookEventPoint::Stop, def(&exit_cmd(1, "unreached"))),
        ]);
        let report = e.run(HookEventPoint::Stop, &input(None)).await;
        assert_eq!(report.verdict, HookVerdict::Block("first".to_owned()));
        assert!(
            report.warnings.is_empty(),
            "later entries do not run: {:?}",
            report.warnings
        );
    }

    // —— timeouts ——

    /// SPEC section 9 acceptance: a timeout force-kills and logs a warning
    /// (kill_on_drop kills the process).
    #[tokio::test]
    async fn timeout_kills_and_warns() {
        let e = engine(&[(
            HookEventPoint::PreToolUse,
            HookDef {
                timeout_ms: 200,
                ..def(&sleep_cmd())
            },
        )]);
        let start = std::time::Instant::now();
        let report = e
            .run(HookEventPoint::PreToolUse, &input(Some("shell")))
            .await;
        assert_eq!(report.verdict, HookVerdict::Allow);
        assert_eq!(report.warnings.len(), 1);
        assert!(report.warnings[0].contains("timed out"));
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "timeouts should return promptly instead of waiting out the command: {:?}",
            start.elapsed()
        );
    }

    // —— once ——

    /// once: fires only once per session (engine instance); matcher misses do
    /// not consume quota.
    #[tokio::test]
    async fn once_fires_only_first_time() {
        let e = engine(&[(
            HookEventPoint::PreToolUse,
            HookDef {
                matcher: Some("shell".to_owned()),
                once: true,
                ..def(&exit_cmd(2, "once-block"))
            },
        )]);
        // Matcher miss: once quota untouched.
        let r = e
            .run(HookEventPoint::PreToolUse, &input(Some("grep")))
            .await;
        assert_eq!(r.verdict, HookVerdict::Allow);
        // First hit: blocked.
        let r = e
            .run(HookEventPoint::PreToolUse, &input(Some("shell")))
            .await;
        assert_eq!(r.verdict, HookVerdict::Block("once-block".to_owned()));
        // Second time: once consumed, no longer runs.
        let r = e
            .run(HookEventPoint::PreToolUse, &input(Some("shell")))
            .await;
        assert_eq!(r.verdict, HookVerdict::Allow);
        assert!(r.warnings.is_empty());
    }

    /// Point parsing: every legal name maps one-to-one, illegal names give None.
    #[test]
    fn event_point_parse_roundtrip() {
        for point in HookEventPoint::ALL {
            assert_eq!(HookEventPoint::parse(point.as_str()), Some(point));
        }
        assert_eq!(HookEventPoint::parse("pre_tool_use"), None);
        assert_eq!(HookEventPoint::parse("Notification"), None);
    }

    // —— timeout normalization / empty commands / static validation ——

    /// `timeout_ms == 0` means "unset": falls back to DEFAULT instead of
    /// timing out immediately.
    #[test]
    fn effective_timeout_maps_zero_to_default() {
        assert_eq!(
            HookDef {
                timeout_ms: 0,
                ..def("true")
            }
            .effective_timeout(),
            Duration::from_millis(DEFAULT_TIMEOUT_MS)
        );
        assert_eq!(
            HookDef {
                timeout_ms: 42,
                ..def("true")
            }
            .effective_timeout(),
            Duration::from_millis(42)
        );
    }

    /// Runtime proof: a fast succeeding command with `timeout_ms == 0` still
    /// allows with no warnings (before the fallback this always reported a
    /// timeout).
    #[tokio::test]
    async fn zero_timeout_falls_back_to_default_at_runtime() {
        let e = engine(&[(
            HookEventPoint::PreToolUse,
            HookDef {
                timeout_ms: 0,
                ..def(&exit_cmd(0, ""))
            },
        )]);
        let report = e
            .run(HookEventPoint::PreToolUse, &input(Some("shell")))
            .await;
        assert_eq!(report.verdict, HookVerdict::Allow);
        assert!(report.warnings.is_empty());
    }

    /// Empty commands: warn and skip without really spawning, and without
    /// consuming the `once` quota (both of two consecutive runs warn — had the
    /// first consumed the quota, the second would skip silently).
    #[tokio::test]
    async fn empty_command_skips_with_warning_without_consuming_once() {
        let e = engine(&[(
            HookEventPoint::PreToolUse,
            HookDef {
                once: true,
                ..def("   ")
            },
        )]);
        for _ in 0..2 {
            let report = e
                .run(HookEventPoint::PreToolUse, &input(Some("shell")))
                .await;
            assert_eq!(report.verdict, HookVerdict::Allow);
            assert_eq!(report.warnings.len(), 1);
            assert!(report.warnings[0].contains("empty command"));
        }
    }

    /// Static validation: flags never-effective entries without running any
    /// command; a clean config yields zero diagnostics.
    #[test]
    fn validate_flags_dead_entries() {
        let e = engine(&[
            (
                HookEventPoint::SessionStart,
                HookDef {
                    matcher: Some("shell".to_owned()),
                    ..def("echo hi")
                },
            ),
            (HookEventPoint::PreToolUse, def("  ")),
            (
                HookEventPoint::PreToolUse,
                HookDef {
                    matcher: Some("  ".to_owned()),
                    ..def(&exit_cmd(0, ""))
                },
            ),
            (
                HookEventPoint::PostToolUse,
                HookDef {
                    timeout_ms: 0,
                    ..def(&exit_cmd(0, ""))
                },
            ),
        ]);
        let diagnostics = e.validate();
        assert_eq!(diagnostics.len(), 4, "{diagnostics:?}");
        assert!(
            diagnostics.iter().any(|d| d.contains("empty command")),
            "{diagnostics:?}"
        );
        assert!(
            diagnostics.iter().any(|d| d.contains("matcher is blank")),
            "{diagnostics:?}"
        );
        assert!(
            diagnostics
                .iter()
                .any(|d| d.contains("carries no tool name")),
            "{diagnostics:?}"
        );
        assert!(
            diagnostics.iter().any(|d| d.contains("timeout_ms is 0")),
            "{diagnostics:?}"
        );

        let clean = engine(&[(HookEventPoint::PreToolUse, def(&exit_cmd(0, "")))]);
        assert!(clean.validate().is_empty());
    }

    // -- plugin middleware --

    /// Plugin middleware runs in registration order after config hooks: each
    /// handler observes the previous output, and `None` drops the event.
    #[test]
    fn plugin_hooks_run_in_registration_order_after_config() {
        use std::sync::Arc;
        let engine = HookEngine::new(HashMap::new());
        let event = serde_json::json!({"n": 1});
        // No handlers: identity pass-through (config behavior unchanged).
        assert_eq!(
            engine.run_plugin_hooks(HookEventPoint::PreToolUse, event.clone()),
            Some(event.clone())
        );
        engine.register_plugin_hook(
            "first",
            HookEventPoint::PreToolUse,
            Arc::new(|mut event| {
                let n = event.get("n").and_then(|v| v.as_u64()).unwrap_or(0);
                event["n"] = serde_json::json!(n + 1);
                Some(event)
            }),
        );
        engine.register_plugin_hook(
            "second",
            HookEventPoint::PreToolUse,
            Arc::new(|mut event| {
                let n = event.get("n").and_then(|v| v.as_u64()).unwrap_or(0);
                event["n"] = serde_json::json!(n * 10);
                Some(event)
            }),
        );
        assert_eq!(engine.plugin_hook_count(HookEventPoint::PreToolUse), 2);
        assert_eq!(engine.plugin_hook_count(HookEventPoint::PostToolUse), 0);
        // (1 + 1) * 10 proves registration order, not config order.
        assert_eq!(
            engine.run_plugin_hooks(HookEventPoint::PreToolUse, event),
            Some(serde_json::json!({"n": 20}))
        );
    }

    /// A `None` return short-circuits the chain (dropped event).
    #[test]
    fn plugin_hook_none_drops_the_event() {
        use std::sync::Arc;
        let engine = HookEngine::new(HashMap::new());
        engine.register_plugin_hook(
            "dropper",
            HookEventPoint::Stop,
            Arc::new(|_| None),
        );
        engine.register_plugin_hook(
            "unreached",
            HookEventPoint::Stop,
            Arc::new(Some),
        );
        assert_eq!(
            engine.run_plugin_hooks(
                HookEventPoint::Stop,
                serde_json::json!({"stop": true})
            ),
            None
        );
    }
}
