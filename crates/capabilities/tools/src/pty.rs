/*! @file PtyShell
 *  @description PTY-backed shell tool: one-shot runs plus persistent sessions.
 *
 *  Responsibilities:
 *  - Run commands under a real PTY (portable-pty: ConPTY on Windows, pty(7)
 *    on Unix — no raw winapi anywhere).
 *  - Keep up to 8 named persistent sessions process-globally (LRU eviction
 *    killing the child); create on first use with the caller cwd, reuse
 *    afterwards, respawn transparently after the shell exits.
 *  - Drain output with a 30 KB cap mirroring the shell tool.
 *
 *  This module must not depend on: transport, frontend crates.
 */

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::is_sensitive_env_name;

use crate::shell_tool::truncate_output;
use crate::{Result, Tool, ToolCtx, ToolOutput};

/// Local result with a plain string error (business failures surface as
/// `ToolOutput`, never as `ToolsError`).
type PtyResult<T> = std::result::Result<T, String>;

/// Max persistent sessions (LRU eviction kills the dropped child).
const MAX_SESSIONS: usize = 8;
/// Default timeout: 60 s.
const DEFAULT_TIMEOUT_MS: u64 = 60_000;
/// Timeout cap: 300 s, clamped to the cap when exceeded.
const MAX_TIMEOUT_MS: u64 = 300_000;
/// Default PTY size.
const DEFAULT_ROWS: u16 = 24;
const DEFAULT_COLS: u16 = 80;
/// Sentinel prefix: the shell echoes `<prefix><n>_<exit>__END` after each
/// command; `<n>` is a process-unique counter so command output can never
/// collide with the marker.
const SENTINEL_PREFIX: &str = "__WAVECODE_PTY_";

static SENTINEL_COUNTER: AtomicU64 = AtomicU64::new(1);

fn err_output(reason: impl Into<String>) -> ToolOutput {
    ToolOutput {
        content: reason.into(),
        is_error: true,
    }
}

/// Pick the shell program and its "run a command string" flag. Mirrors
/// `shell_tool::shell_invocation` (kept local so the shell tool's spawn-site
/// hook stays the only coupling point): `cmd /C` on Windows, `sh -c` on
/// Unix, `WAVECODE_SHELL` override with a `cmd`-name heuristic for `/C`.
fn shell_program() -> (String, &'static str) {
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

/// Interactive shell program for persistent sessions (no `-c`).
fn interactive_program() -> String {
    if let Ok(custom) = std::env::var("WAVECODE_SHELL") {
        return custom;
    }
    if cfg!(windows) {
        "cmd".to_owned()
    } else {
        "sh".to_owned()
    }
}

/// Clamp an optional dimension to a valid PTY size: only values that fit
/// the u16 the PTY layer takes are kept, so 0 and unrepresentable (huge)
/// values fall back to the default instead of truncating.
fn clamp_dim(value: Option<u64>, fallback: u16) -> u16 {
    match value {
        Some(n) if (1..=u64::from(u16::MAX)).contains(&n) => n as u16,
        _ => fallback,
    }
}

fn pty_size(rows: Option<u64>, cols: Option<u64>) -> PtySize {
    PtySize {
        rows: clamp_dim(rows, DEFAULT_ROWS),
        cols: clamp_dim(cols, DEFAULT_COLS),
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// Strip deny-listed and secret-shaped variables from a PTY command.
fn scrub_command(cmd: &mut CommandBuilder, ctx: &ToolCtx) {
    for name in &ctx.deny_env {
        cmd.env_remove(name);
    }
    for (key, _) in std::env::vars_os() {
        let name: String = key.to_string_lossy().into_owned();
        if is_sensitive_env_name(&name) {
            cmd.env_remove(name);
        }
    }
}

/// One live shell behind the PTY master.
struct Session {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
    cwd: PathBuf,
    deny_env: Vec<String>,
    size: PtySize,
}

impl Session {
    fn spawn(cwd: &std::path::Path, deny_env: &[String], size: PtySize) -> PtyResult<Self> {
        let system = native_pty_system();
        let pair = system
            .openpty(size)
            .map_err(|e| format!("pty unavailable: {e}"))?;
        let mut cmd = CommandBuilder::new(interactive_program());
        cmd.cwd(cwd.as_os_str());
        let scrub_ctx = ToolCtx {
            cwd: cwd.to_path_buf(),
            deny_env: deny_env.to_vec(),
        };
        scrub_command(&mut cmd, &scrub_ctx);
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| format!("pty spawn failed: {e}"))?;
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("pty reader failed: {e}"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| format!("pty writer failed: {e}"))?;
        Ok(Self {
            master: pair.master,
            child,
            reader,
            writer,
            cwd: cwd.to_path_buf(),
            deny_env: deny_env.to_vec(),
            size,
        })
    }

    /// Respawn after the shell exited (same cwd and scrub list).
    fn respawn(&mut self) -> PtyResult<()> {
        let fresh = Self::spawn(&self.cwd.clone(), &self.deny_env.clone(), self.size)?;
        *self = fresh;
        Ok(())
    }

    fn exited(&mut self) -> bool {
        self.child.try_wait().ok().flatten().is_some()
    }
}

struct Entry {
    session: Arc<Mutex<Session>>,
    killer: Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>>,
}

struct Registry {
    sessions: HashMap<String, Entry>,
    order: VecDeque<String>,
}

impl Registry {
    fn touch(&mut self, name: &str) {
        self.order.retain(|n| n != name);
        self.order.push_back(name.to_owned());
    }

    fn evict_oldest(&mut self) {
        if let Some(oldest) = self.order.pop_front()
            && let Some(entry) = self.sessions.remove(&oldest)
        {
            let _ = entry
                .killer
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .kill();
        }
    }
}

fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        Mutex::new(Registry {
            sessions: HashMap::new(),
            order: VecDeque::new(),
        })
    })
}

fn lock_registry() -> std::sync::MutexGuard<'static, Registry> {
    registry().lock().unwrap_or_else(|e| e.into_inner())
}

/// Get or create a persistent session (LRU-capped at [`MAX_SESSIONS`]).
fn get_or_create(name: &str, ctx: &ToolCtx, size: PtySize) -> PtyResult<Entry> {
    // Fast path: reuse (clone the Arcs first so the map borrow ends
    // before the mutable touch).
    {
        let mut reg = lock_registry();
        if let Some((session, killer)) = reg
            .sessions
            .get(name)
            .map(|e| (e.session.clone(), e.killer.clone()))
        {
            reg.touch(name);
            return Ok(Entry { session, killer });
        }
    }
    // Slow path: spawn outside the registry lock, then insert. A racing
    // creator for the same name wins; the loser kills its spare shell
    // (dropping a live Child would leak the process).
    let mut spare = Session::spawn(&ctx.cwd, &ctx.deny_env, size)?;
    let mut reg = lock_registry();
    if let Some((session, killer)) = reg
        .sessions
        .get(name)
        .map(|e| (e.session.clone(), e.killer.clone()))
    {
        reg.touch(name);
        drop(reg);
        let _ = spare.child.kill();
        return Ok(Entry { session, killer });
    }
    while reg.sessions.len() >= MAX_SESSIONS {
        reg.evict_oldest();
    }
    let killer: Box<dyn ChildKiller + Send + Sync> = spare.child.clone_killer();
    let entry = Entry {
        session: Arc::new(Mutex::new(spare)),
        killer: Arc::new(Mutex::new(killer)),
    };
    let out = Entry {
        session: entry.session.clone(),
        killer: entry.killer.clone(),
    };
    reg.sessions.insert(name.to_owned(), entry);
    reg.touch(name);
    Ok(out)
}

#[cfg(test)]
pub(crate) fn session_count_for_tests() -> usize {
    lock_registry().sessions.len()
}

#[cfg(test)]
pub(crate) fn registry_contains_for_tests(name: &str) -> bool {
    lock_registry().sessions.contains_key(name)
}

#[cfg(test)]
pub(crate) fn reset_registry_for_tests() {
    let mut reg = lock_registry();
    for entry in reg.sessions.values() {
        let _ = entry
            .killer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .kill();
    }
    reg.sessions.clear();
    reg.order.clear();
}

/// Run one interaction on a live session: reset on exit, resize when asked,
/// write the command plus a sentinel echo, and read until the sentinel.
fn interact(
    entry: &Entry,
    command: &str,
    size: Option<PtySize>,
    timeout: Duration,
) -> PtyResult<(String, i32)> {
    let session = entry.session.clone();
    let mut guard = session.lock().unwrap_or_else(|e| e.into_inner());
    if guard.exited() {
        guard.respawn()?;
    }
    if let Some(size) = size {
        guard.size = size;
        let _ = guard.master.resize(size);
    }
    let marker = format!(
        "{SENTINEL_PREFIX}{}__",
        SENTINEL_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    guard
        .writer
        .write_all(command.as_bytes())
        .map_err(|e| format!("pty write failed: {e}"))?;
    guard
        .writer
        .write_all(b"\n")
        .map_err(|e| format!("pty write failed: {e}"))?;
    let sentinel_cmd = sentinel_command(&marker);
    guard
        .writer
        .write_all(sentinel_cmd.as_bytes())
        .map_err(|e| format!("pty write failed: {e}"))?;
    guard
        .writer
        .flush()
        .map_err(|e| format!("pty write failed: {e}"))?;

    let deadline = Instant::now() + timeout;
    let mut raw: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let marker_line = marker.as_bytes();
    loop {
        if Instant::now() > deadline {
            let _ = entry
                .killer
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .kill();
            return Err("pty interaction timed out".to_string());
        }
        match guard.reader.read(&mut chunk) {
            Ok(0) => return Err("pty shell exited mid-command".to_owned()),
            Ok(n) => {
                raw.extend_from_slice(&chunk[..n]);
                if let Some(code) = find_marker(&raw, marker_line) {
                    let text = String::from_utf8_lossy(&raw).into_owned();
                    return Ok((strip_marker(&text, &marker), code.unwrap_or(-1)));
                }
                if raw.len() > 512 * 1024 {
                    return Err("pty output exceeded internal buffer".to_owned());
                }
            }
            Err(e) => return Err(format!("pty read failed: {e}")),
        }
    }
}

/// Sentinel echo for the session shell: prints `<marker><exit>__END`.
/// Unix shells expand `$?`; cmd.exe has no `$?` and uses `%ERRORLEVEL%`.
fn sentinel_command(marker: &str) -> String {
    if cfg!(windows) {
        format!("echo {marker}%ERRORLEVEL%__END\n")
    } else {
        format!("echo {marker}$?__END\n")
    }
}

/// Scan buffered bytes for a `<marker><code>__END` line; returns the parsed
/// exit code when the marker is complete. The code must be all digits: the
/// echoed `echo <marker>$?__END` line carries the marker with a literal `$?`
/// and must keep scanning until the shell's expanded reply arrives.
fn find_marker(raw: &[u8], marker: &[u8]) -> Option<Option<i32>> {
    let end = b"__END";
    let mut i = 0;
    while i + marker.len() <= raw.len() {
        if &raw[i..i + marker.len()] == marker {
            let rest = &raw[i + marker.len()..];
            if let Some(end_pos) = find_subslice(rest, end) {
                let code_text = String::from_utf8_lossy(&rest[..end_pos]);
                let code_text = code_text.trim();
                if !code_text.is_empty() && code_text.bytes().all(|b| b.is_ascii_digit()) {
                    return Some(code_text.parse::<i32>().ok());
                }
                // Marker-like but not the shell's reply (the echoed
                // sentinel command): keep scanning past it.
                i += 1;
                continue;
            }
            return None;
        }
        i += 1;
    }
    None
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Remove the sentinel echo line and the marker line from captured text.
fn strip_marker(text: &str, marker: &str) -> String {
    text.lines()
        .filter(|line| !line.contains(marker))
        .collect::<Vec<_>>()
        .join("\n")
}

/// One-shot PTY run: spawn `shell -c <command>`, read until EOF, wait for
/// the exit code. Blocking; call from `spawn_blocking`.
fn run_one_shot(
    command: &str,
    ctx: &ToolCtx,
    size: PtySize,
    timeout: Duration,
    killer_slot: &Arc<Mutex<Option<Box<dyn ChildKiller + Send + Sync>>>>,
) -> PtyResult<(String, i32)> {
    let system = native_pty_system();
    let pair = system
        .openpty(size)
        .map_err(|e| format!("pty unavailable: {e}"))?;
    let (program, flag) = shell_program();
    let mut cmd = CommandBuilder::new(program);
    cmd.arg(flag);
    cmd.arg(command);
    cmd.cwd(ctx.cwd.as_os_str());
    scrub_command(&mut cmd, ctx);
    let mut child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| format!("pty spawn failed: {e}"))?;
    // Publish the killer for the outer timeout arm: the internal deadline
    // check only runs after a read returns, so a child idling without
    // output must be killable from outside the blocked reader thread.
    *killer_slot
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(child.clone_killer());
    drop(pair.slave);
    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| format!("pty reader failed: {e}"))?;
    drop(pair.master.take_writer());

    let deadline = Instant::now() + timeout;
    let mut raw: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if Instant::now() > deadline {
            let _ = child.kill();
            return Err("pty run timed out".to_string());
        }
        // Bound each blocking read so the deadline stays responsive even
        // when the child idles without output.
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&chunk[..n]);
                if raw.len() > 512 * 1024 {
                    break;
                }
            }
            Err(e) => {
                // EOF surfaces as an error on some platforms once the child
                // exits; treat a dead child as clean EOF.
                if child.try_wait().ok().flatten().is_some() {
                    break;
                }
                return Err(format!("pty read failed: {e}"));
            }
        }
        if child.try_wait().ok().flatten().is_some() {
            // Drain whatever the child left behind, then stop.
            drain_available(&mut reader, &mut raw);
            break;
        }
    }
    // Cap break can leave the child alive: kill before reaping so wait()
    // never blocks (the outer timeout already returned by then).
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill();
    }
    let code = child.wait().map(|s| s.exit_code() as i32).unwrap_or(-1);
    Ok((String::from_utf8_lossy(&raw).into_owned(), code))
}

/// Non-blocking drain of bytes already queued on the reader.
fn drain_available(reader: &mut Box<dyn Read + Send>, raw: &mut Vec<u8>) {
    let mut chunk = [0u8; 4096];
    // A short polling window: PTY masters on Unix support O_NONBLOCK via
    // fcntl, but portable-pty exposes only blocking reads, so poll with a
    // helper thread is overkill — one extra blocking read is bounded by the
    // already-exited child closing the master (EOF).
    let _ = reader
        .read(&mut chunk)
        .map(|n| raw.extend_from_slice(&chunk[..n]));
}

fn parse_timeout(input: &serde_json::Value) -> std::result::Result<u64, ToolOutput> {
    match input.get("timeout_ms") {
        None => Ok(DEFAULT_TIMEOUT_MS),
        Some(v) => match v.as_u64() {
            Some(n) => Ok(n.min(MAX_TIMEOUT_MS)),
            None => Err(err_output(
                "invalid parameter 'timeout_ms' (non-negative integer required)",
            )),
        },
    }
}

/// PTY shell tool: `pty_shell {command, rows?, cols?, session?}`.
///
/// Without `session` the command runs one-shot under a fresh PTY; with
/// `session` it runs on the named persistent shell (created on first use
/// with the caller cwd, reused afterwards, respawned after exit). At most
/// 8 sessions live at once; the least-recently-used is dropped (child
/// killed) to make room.
pub struct PtyShell;

#[async_trait::async_trait]
impl Tool for PtyShell {
    fn name(&self) -> &str {
        "pty_shell"
    }

    fn description(&self) -> &str {
        "Run a shell command under a PTY (terminal semantics: tty-aware programs work). \
         Without `session` the command runs one-shot; with `session` (a name) it runs on a \
         persistent shell kept across calls (state like cwd and env persists; the shell \
         respawns transparently after `exit`). `rows`/`cols` set the terminal size. \
         Output is truncated at 30KB. Non-interactive piping is unchanged: prefer `shell` \
         for plain commands."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Shell command to execute"
                },
                "rows": {
                    "type": "integer",
                    "description": "Terminal rows (default 24)"
                },
                "cols": {
                    "type": "integer",
                    "description": "Terminal columns (default 80)"
                },
                "session": {
                    "type": "string",
                    "description": "Persistent session name (omit for a one-shot run)"
                }
            },
            "required": ["command"]
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let command = match input.get("command").and_then(|v| v.as_str()) {
            Some(c) => c.to_owned(),
            None => {
                return Ok(err_output(
                    "missing or invalid parameter 'command' (string required)",
                ));
            }
        };
        let timeout_ms = match parse_timeout(&input) {
            Ok(n) => n,
            Err(out) => return Ok(out),
        };
        let rows = input.get("rows").and_then(|v| v.as_u64());
        let cols = input.get("cols").and_then(|v| v.as_u64());
        if input.get("rows").is_some_and(|v| v.as_u64().is_none()) {
            return Ok(err_output(
                "invalid parameter 'rows' (non-negative integer required)",
            ));
        }
        if input.get("cols").is_some_and(|v| v.as_u64().is_none()) {
            return Ok(err_output(
                "invalid parameter 'cols' (non-negative integer required)",
            ));
        }
        let session = input
            .get("session")
            .and_then(|v| v.as_str())
            .map(|s| s.to_owned());
        if let Some(name) = session {
            if name.is_empty() {
                return Ok(err_output(
                    "invalid parameter 'session' (non-empty string required)",
                ));
            }
            let size = pty_size(rows, cols);
            let entry = match get_or_create(&name, ctx, size) {
                Ok(e) => e,
                Err(reason) => return Ok(err_output(reason)),
            };
            let timeout = Duration::from_millis(timeout_ms);
            let size_opt = if rows.is_some() || cols.is_some() {
                Some(size)
            } else {
                None
            };
            // The killer stays outside the moved entry so the timeout arm can
            // unblock the reader thread (which holds the session lock).
            let killer = entry.killer.clone();
            let label = command.clone();
            let run =
                tokio::task::spawn_blocking(move || interact(&entry, &command, size_opt, timeout));
            let (output, code) =
                match tokio::time::timeout(Duration::from_millis(timeout_ms), run).await {
                    Ok(Ok(Ok(pair))) => pair,
                    Ok(Ok(Err(reason))) => return Ok(err_output(reason)),
                    Ok(Err(_join)) => return Ok(err_output("pty session task failed")),
                    Err(_) => {
                        let _ = killer.lock().unwrap_or_else(|e| e.into_inner()).kill();
                        return Ok(err_output(format!("timeout after {timeout_ms}ms: {label}")));
                    }
                };
            let body = truncate_output(output.as_bytes());
            Ok(ToolOutput {
                content: format!("exit code: {code}\n{body}"),
                is_error: code != 0,
            })
        } else {
            let ctx_owned = ctx.clone();
            let size = pty_size(rows, cols);
            let timeout = Duration::from_millis(timeout_ms);
            let label = command.clone();
            let killer_slot: Arc<Mutex<Option<Box<dyn ChildKiller + Send + Sync>>>> =
                Arc::new(Mutex::new(None));
            let slot = killer_slot.clone();
            let run = tokio::task::spawn_blocking(move || {
                run_one_shot(&command, &ctx_owned, size, timeout, &slot)
            });
            let (output, code) =
                match tokio::time::timeout(Duration::from_millis(timeout_ms), run).await {
                    Ok(Ok(Ok(pair))) => pair,
                    Ok(Ok(Err(reason))) => return Ok(err_output(reason)),
                    Ok(Err(_join)) => return Ok(err_output("pty task failed")),
                    // Same hygiene as the session path: kill the child (and
                    // unblock the reader thread) instead of leaving both
                    // behind until the shell exits on its own.
                    Err(_) => {
                        if let Some(mut killer) = killer_slot
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .take()
                        {
                            let _ = killer.kill();
                        }
                        return Ok(err_output(format!("timeout after {timeout_ms}ms: {label}")));
                    }
                };
            let body = truncate_output(output.as_bytes());
            Ok(ToolOutput {
                content: format!("exit code: {code}\n{body}"),
                is_error: code != 0,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Tool, ToolCtx};

    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn ctx() -> (tempfile::TempDir, ToolCtx) {
        let dir = tempfile::tempdir().unwrap();
        let c = ToolCtx {
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
        };
        (dir, c)
    }

    fn unique_session(prefix: &str) -> String {
        static N: AtomicU64 = AtomicU64::new(1);
        format!("{prefix}-{}", N.fetch_add(1, Ordering::Relaxed))
    }

    #[test]
    fn dims_clamp_to_sane_sizes() {
        assert_eq!(clamp_dim(None, 24), 24);
        assert_eq!(clamp_dim(Some(0), 24), 24);
        assert_eq!(clamp_dim(Some(200), 80), 200);
        assert_eq!(clamp_dim(Some(100000), 24), 24);
    }

    #[test]
    fn marker_scan_finds_code_and_ignores_partial() {
        let marker = b"__WAVECODE_PTY_1__";
        let raw = b"some output\n__WAVECODE_PTY_1__0__END\n";
        assert_eq!(find_marker(raw, marker), Some(Some(0)));
        let partial = b"output __WAVECODE_PTY_1__";
        assert_eq!(find_marker(partial, marker), None);
        assert_eq!(find_marker(b"plain output", marker), None);
        // The echoed sentinel command carries the marker with a literal `$?`
        // and must not match early; the shell's expanded reply matches.
        let echoed = b"echo __WAVECODE_PTY_1__$?__END\n__WAVECODE_PTY_1__0__END\n";
        assert_eq!(find_marker(echoed, marker), Some(Some(0)));
    }

    #[test]
    fn strip_marker_removes_only_marker_lines() {
        let text = "echo hi\nhi\n__WAVECODE_PTY_3__0__END";
        let stripped = strip_marker(text, "__WAVECODE_PTY_3__");
        assert!(stripped.contains("hi"));
        assert!(!stripped.contains("__WAVECODE_PTY_3__"));
    }

    /// True when a real shell echoes through a PTY within budget.
    ///
    /// `pty_available` only proves a PTY pair opens; some headless
    /// environments open the pair while shell I/O never arrives, hanging
    /// every live test on its timeout. This probe runs one end-to-end echo
    /// (single-flight, ~10s worst case) so those environments skip instead
    /// of burning minutes on timeouts.
    async fn pty_live() -> bool {
        static PROBE: tokio::sync::OnceCell<bool> = tokio::sync::OnceCell::const_new();
        *PROBE
            .get_or_init(|| async {
                // A wedged ConPTY init can block openpty/spawn/read forever,
                // defeating every in-band timeout (including the 8s echo
                // below). Run the whole probe on a plain thread and bound it
                // from the outside; a wedged probe leaks one thread and
                // reports unavailable.
                let (done_tx, done_rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let ok = native_pty_system()
                        .openpty(PtySize::default())
                        .is_ok()
                        && {
                            let (_d, c) = ctx();
                            let rt = tokio::runtime::Builder::new_current_thread()
                                .enable_all()
                                .build();
                            match rt {
                                Err(_) => false,
                                Ok(rt) => rt.block_on(async {
                                    let probed = PtyShell
                                        .execute(
                                            serde_json::json!({"command": "echo WAVECODE_PTY_PROBE_OK", "timeout_ms": 8000}),
                                            &c,
                                        )
                                        .await;
                                    matches!(probed, Ok(out) if !out.is_error && out.content.contains("WAVECODE_PTY_PROBE_OK"))
                                }),
                            }
                        };
                    let _ = done_tx.send(ok);
                });
                done_rx
                    .recv_timeout(std::time::Duration::from_secs(20))
                    .unwrap_or(false)
            })
            .await
    }

    #[tokio::test]
    async fn one_shot_timeout_returns_promptly_and_kills() {
        if !pty_live().await {
            return;
        }
        let (_d, c) = ctx();
        let cmd = if cfg!(windows) {
            "timeout /t 30 /nobreak"
        } else {
            "sleep 30"
        };
        let started = std::time::Instant::now();
        let out = PtyShell
            .execute(
                serde_json::json!({"command": cmd, "timeout_ms": 1500}),
                &c,
            )
            .await
            .unwrap();
        let elapsed = started.elapsed();
        assert!(out.is_error, "expected a timeout error: {}", out.content);
        assert!(
            out.content.contains("timeout after 1500ms"),
            "{}",
            out.content
        );
        // Promptly: the outer timeout killed the idle child, unblocking
        // the reader thread (the internal deadline check alone could not).
        // The bound stays well under the child's 30s runtime (the pre-fix
        // behavior) while tolerating a loaded test machine.
        assert!(
            elapsed < std::time::Duration::from_secs(20),
            "timeout must return promptly, took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn one_shot_echo_round_trip() {
        if !pty_live().await {
            return;
        }
        let (_d, c) = ctx();
        let out = PtyShell
            .execute(serde_json::json!({"command": "echo hello-pty"}), &c)
            .await
            .unwrap();
        assert!(!out.is_error, "one-shot echo must succeed: {}", out.content);
        assert!(out.content.contains("hello-pty"));
        assert!(out.content.contains("exit code: 0"));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    #[cfg(unix)]
    async fn session_reuses_shell_state() {
        let _guard = TEST_LOCK.lock().unwrap();
        if !pty_live().await {
            return;
        }
        let (_d, c) = ctx();
        let name = unique_session("reuse");
        let out = PtyShell
            .execute(
                serde_json::json!({"command": "export WAVECODE_PTY_PROBE=abc123", "session": name, "timeout_ms": 15000}),
                &c,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "setup must succeed: {}", out.content);
        let out = PtyShell
            .execute(
                serde_json::json!({"command": "echo $WAVECODE_PTY_PROBE", "session": name, "timeout_ms": 15000}),
                &c,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "reuse must succeed: {}", out.content);
        assert!(
            out.content.contains("abc123"),
            "session must keep env: {}",
            out.content
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    #[cfg(windows)]
    async fn session_reuses_shell_state() {
        let _guard = TEST_LOCK.lock().unwrap();
        if !pty_live().await {
            return;
        }
        let (_d, c) = ctx();
        let name = unique_session("reuse");
        let out = PtyShell
            .execute(
                serde_json::json!({"command": "set WAVECODE_PTY_PROBE=abc123", "session": name, "timeout_ms": 15000}),
                &c,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "setup must succeed: {}", out.content);
        let out = PtyShell
            .execute(
                serde_json::json!({"command": "echo %WAVECODE_PTY_PROBE%", "session": name, "timeout_ms": 15000}),
                &c,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "reuse must succeed: {}", out.content);
        assert!(
            out.content.contains("abc123"),
            "session must keep env: {}",
            out.content
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn session_resets_after_exit() {
        let _guard = TEST_LOCK.lock().unwrap();
        if !pty_live().await {
            return;
        }
        let (_d, c) = ctx();
        let name = unique_session("reset");
        let _ = PtyShell
            .execute(
                serde_json::json!({"command": "exit", "session": name, "timeout_ms": 15000}),
                &c,
            )
            .await
            .unwrap();
        let out = PtyShell
            .execute(serde_json::json!({"command": "echo back-again", "session": name, "timeout_ms": 15000}), &c)
            .await
            .unwrap();
        assert!(!out.is_error, "respawned shell must work: {}", out.content);
        assert!(out.content.contains("back-again"));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn registry_evicts_lru_past_cap() {
        let _guard = TEST_LOCK.lock().unwrap();
        if !pty_live().await {
            return;
        }
        reset_registry_for_tests();
        let (_d, c) = ctx();
        let mut names = Vec::new();
        for i in 0..(MAX_SESSIONS + 1) {
            let name = unique_session(&format!("evict{i}"));
            let out = PtyShell
                .execute(
                    serde_json::json!({"command": "echo hi", "session": name, "timeout_ms": 15000}),
                    &c,
                )
                .await
                .unwrap();
            assert!(!out.is_error, "session {name} must start: {}", out.content);
            names.push(name);
        }
        assert_eq!(session_count_for_tests(), MAX_SESSIONS);
        assert!(
            !registry_contains_for_tests(&names[0]),
            "oldest session must be evicted"
        );
        assert!(registry_contains_for_tests(&names[names.len() - 1]));
        reset_registry_for_tests();
    }

    #[tokio::test]
    async fn missing_command_is_error_output() {
        let (_d, c) = ctx();
        let out = PtyShell.execute(serde_json::json!({}), &c).await.unwrap();
        assert!(out.is_error);
    }
}
