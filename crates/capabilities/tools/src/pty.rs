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
 *  - Release every pseudoconsole it opens, on every path that can reach it.
 *
 *  Windows console lifetime (measured on a host where ConPTY children cannot
 *  start at all): the only thing that terminates `conhost.exe --headless` is
 *  `ClosePseudoConsole` — portable-pty's `Drop` for the master. A process
 *  that merely exits while a console is open leaves the conhost behind, and
 *  an orphaned headless conhost spins at ~40% of a core indefinitely (17 of
 *  them saturated a 16-thread machine). The release itself blocks for
 *  97–188 s while such a console tears down, so it never runs on a caller's
 *  response path: consoles live in a shared slot and are released either
 *  inline (a path that returns) or detached (a timeout arm, a wedged probe,
 *  an eviction). The tool therefore probes host capability once per process
 *  and, when ConPTY cannot start children, refuses with that reason instead
 *  of opening one wedged console per call.
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

/// Shared handle to the master side of one pseudoconsole.
///
/// The console must be released by `ClosePseudoConsole` (portable-pty's
/// `Drop` for the master): a process that merely exits while a console is
/// open leaves `conhost.exe --headless` behind, and on hosts where ConPTY
/// child processes fail to start that conhost spins at ~40% of a core
/// indefinitely. Measured on such a host: the release call itself *blocks*
/// for 97–188 s while the wedged console tears down, so the handle lives in a
/// shared slot that a supervisor (the tool's timeout arm) can reach even
/// while a reader thread is blocked inside the console, and the release
/// always happens off the caller's path.
type MasterSlot = Arc<Mutex<Option<Box<dyn MasterPty + Send>>>>;

/// Take the master out of `slot`; the caller owns the (possibly blocking)
/// release from here.
fn take_master(slot: &MasterSlot) -> Option<Box<dyn MasterPty + Send>> {
    slot.lock().unwrap_or_else(|e| e.into_inner()).take()
}

/// Release one owned master off the caller's path (the form used everywhere:
/// see [`release_master_detached`] for why waiting is never an option).
fn release_master_owned(master: Box<dyn MasterPty + Send>) {
    std::thread::spawn(move || drop(master));
}

/// Release a pseudoconsole off the caller's path (detached plain thread).
///
/// This is the only release form the tool uses. `ClosePseudoConsole` (the
/// master's drop) blocks until the console exits, which on a wedged console
/// means minutes to hours (measured: >13 min); a thread that waits on it
/// — a tokio blocking task included, which its runtime then waits for —
/// hangs whatever called it. The detached thread instead lives as long as the
/// release needs, and a healthy console closes in milliseconds.
fn release_master_detached(slot: &MasterSlot) {
    if let Some(master) = take_master(slot) {
        release_master_owned(master);
    }
}

/// One live shell behind the PTY master.
struct Session {
    master: MasterSlot,
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
        // Publish the console into the slot first: from here on the handle is
        // reachable even if a later step wedges.
        let master: MasterSlot = Arc::new(Mutex::new(Some(pair.master)));
        let (reader, writer) = {
            let guard = master.lock().unwrap_or_else(|e| e.into_inner());
            let master = guard
                .as_ref()
                .ok_or_else(|| "pty master missing".to_string())?;
            (
                master
                    .try_clone_reader()
                    .map_err(|e| format!("pty reader failed: {e}"))?,
                master
                    .take_writer()
                    .map_err(|e| format!("pty writer failed: {e}"))?,
            )
        };
        Ok(Self {
            master,
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
        // The freshly opened console goes into OUR slot (the registry's entry
        // shares it, so its identity must survive); the old one is released
        // off-thread — a wedged console would otherwise block this call for
        // minutes.
        let fresh_master = take_master(&fresh.master);
        let old = std::mem::replace(
            &mut *self.master.lock().unwrap_or_else(|e| e.into_inner()),
            fresh_master,
        );
        if let Some(old) = old {
            release_master_owned(old);
        }
        self.child = fresh.child;
        self.reader = fresh.reader;
        self.writer = fresh.writer;
        Ok(())
    }

    fn exited(&mut self) -> bool {
        self.child.try_wait().ok().flatten().is_some()
    }
}

struct Entry {
    session: Arc<Mutex<Session>>,
    killer: Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>>,
    /// The session's pseudoconsole, shared so a supervisor can release it
    /// without taking the session lock (a reader blocked inside the console
    /// holds that lock).
    master: MasterSlot,
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
            // Off-thread: this runs under the registry lock, and a wedged
            // console's release blocks for minutes.
            release_master_detached(&entry.master);
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
        if let Some((session, killer, master)) = reg
            .sessions
            .get(name)
            .map(|e| (e.session.clone(), e.killer.clone(), e.master.clone()))
        {
            reg.touch(name);
            return Ok(Entry {
                session,
                killer,
                master,
            });
        }
    }
    // Slow path: spawn outside the registry lock, then insert. A racing
    // creator for the same name wins; the loser kills its spare shell and
    // releases its console (dropping a live Child would leak the process).
    let mut spare = Session::spawn(&ctx.cwd, &ctx.deny_env, size)?;
    let mut reg = lock_registry();
    if let Some((session, killer, master)) = reg
        .sessions
        .get(name)
        .map(|e| (e.session.clone(), e.killer.clone(), e.master.clone()))
    {
        reg.touch(name);
        drop(reg);
        let _ = spare.child.kill();
        release_master_detached(&spare.master);
        return Ok(Entry {
            session,
            killer,
            master,
        });
    }
    while reg.sessions.len() >= MAX_SESSIONS {
        reg.evict_oldest();
    }
    let killer: Box<dyn ChildKiller + Send + Sync> = spare.child.clone_killer();
    let master = spare.master.clone();
    let entry = Entry {
        session: Arc::new(Mutex::new(spare)),
        killer: Arc::new(Mutex::new(killer)),
        master: master.clone(),
    };
    let out = Entry {
        session: entry.session.clone(),
        killer: entry.killer.clone(),
        master,
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
    let mut released = Vec::new();
    for entry in reg.sessions.values() {
        let _ = entry
            .killer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .kill();
        released.push(entry.master.clone());
    }
    reg.sessions.clear();
    reg.order.clear();
    drop(reg);
    // Off-thread for the same reason as everywhere else: a wedged console's
    // close blocks for minutes to hours, and a test thread that waits on it
    // hangs the run. Healthy consoles close in milliseconds, well before the
    // process exits.
    for slot in &released {
        release_master_detached(slot);
    }
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
        let master = guard.master.clone();
        let guard = master.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(master) = guard.as_ref() {
            let _ = master.resize(size);
        }
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
///
/// Every path this task actually reaches closes the console before
/// returning (blocking: fast on a healthy host, and the caller must not
/// report success while a console is still unreleased). A task that is
/// abandoned mid-read never returns — the tool's timeout arm closes the
/// console from outside instead, which is why the slot is shared.
fn run_one_shot(
    command: &str,
    ctx: &ToolCtx,
    size: PtySize,
    timeout: Duration,
    killer_slot: &Arc<Mutex<Option<Box<dyn ChildKiller + Send + Sync>>>>,
    master_slot: &MasterSlot,
) -> PtyResult<(String, i32)> {
    let outcome = run_one_shot_inner(command, ctx, size, timeout, killer_slot, master_slot);
    release_master_detached(master_slot);
    outcome
}

fn run_one_shot_inner(
    command: &str,
    ctx: &ToolCtx,
    size: PtySize,
    timeout: Duration,
    killer_slot: &Arc<Mutex<Option<Box<dyn ChildKiller + Send + Sync>>>>,
    master_slot: &MasterSlot,
) -> PtyResult<(String, i32)> {
    let system = native_pty_system();
    let pair = system
        .openpty(size)
        .map_err(|e| format!("pty unavailable: {e}"))?;
    // Publish the console into the shared slot before anything can wedge: the
    // timeout arm reaches it from outside this (possibly abandoned) task, and
    // a console that is never closed outlives the process as a spinning
    // conhost.
    *master_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(pair.master);
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
    *killer_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(child.clone_killer());
    drop(pair.slave);
    let mut reader = {
        let guard = master_slot.lock().unwrap_or_else(|e| e.into_inner());
        let master = guard
            .as_ref()
            .ok_or_else(|| "pty master missing".to_string())?;
        master
            .try_clone_reader()
            .map_err(|e| format!("pty reader failed: {e}"))?
    };
    let writer = {
        let guard = master_slot.lock().unwrap_or_else(|e| e.into_inner());
        let master = guard
            .as_ref()
            .ok_or_else(|| "pty master missing".to_string())?;
        master.take_writer()
    };
    drop(writer);

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

/// Answer whether this host can actually run a ConPTY child, probing once
/// per process and remembering the verdict on disk.
///
/// Some Windows hosts fail every ConPTY child with `STATUS_DLL_INIT_FAILED`
/// (observed as exit code 3221225794 = 0xC0000142) while plain pipe spawns
/// work normally. On such a host every attempt additionally leaves a console
/// spinning until it is closed — and the close itself can block for many
/// minutes or longer (measured: 97 s, 188 s, and >13 min in one case). So the
/// attempt is never repeated lightly: the verdict is cached in-process and,
/// when negative, on disk for [`PTY_MARKER_TTL`], which keeps later runs
/// (tests included) from opening a console at all. Deleting the marker
/// retries immediately.
static HOST_PTY: tokio::sync::OnceCell<bool> = tokio::sync::OnceCell::const_new();

/// Nonce echoed by the capability probe; its absence means the console never
/// ran the command.
const PTY_PROBE_NONCE: &str = "WAVECODE_PTY_PROBE_OK";

/// How long a recorded negative verdict suppresses further probes.
const PTY_MARKER_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// File recording that this host's ConPTY cannot start children.
fn pty_marker_path() -> Option<PathBuf> {
    let home = wavecode_config::home_dir()?;
    Some(home.join(".wavecode").join("pty-unavailable"))
}

/// Whether a fresh negative verdict is on disk (so no probe should run).
fn pty_marker_fresh() -> bool {
    let Some(path) = pty_marker_path() else {
        return false;
    };
    std::fs::metadata(&path)
        .and_then(|meta| meta.modified())
        .map(|modified| {
            modified
                .elapsed()
                .map(|age| age < PTY_MARKER_TTL)
                .unwrap_or(true)
        })
        .unwrap_or(false)
}

/// Record the negative verdict (best-effort: a read-only home only means the
/// next process probes again).
fn record_pty_unavailable(reason: &str) {
    if let Some(path) = pty_marker_path()
        && let Some(dir) = path.parent()
    {
        let _ = std::fs::create_dir_all(dir);
        let _ = std::fs::write(
            &path,
            format!(
                "ConPTY children cannot start on this host; pty_shell reports this instead of \
                 opening a console that would spin.\nrecorded: {reason}\ndelete this file to \
                 probe again (automatic after {} h).\n",
                PTY_MARKER_TTL.as_secs() / 3600
            ),
        );
    }
}

/// Clear the marker after a successful probe (a repaired host recovers).
fn clear_pty_marker() {
    if let Some(path) = pty_marker_path() {
        let _ = std::fs::remove_file(path);
    }
}

async fn host_pty_supported(ctx: &ToolCtx) -> bool {
    *HOST_PTY
        .get_or_init(|| async {
            if pty_marker_fresh() {
                return false;
            }
            let ctx = ctx.clone();
            let killer_slot: Arc<Mutex<Option<Box<dyn ChildKiller + Send + Sync>>>> =
                Arc::new(Mutex::new(None));
            let master_slot: MasterSlot = Arc::new(Mutex::new(None));
            let killer = killer_slot.clone();
            let master = master_slot.clone();
            // A plain thread, not a runtime blocking task: a wedged probe never
            // returns, and a runtime waits for its blocking tasks when it
            // shuts down — which is exactly how a hung test process (and a
            // hung runtime at exit) looked before. This thread is allowed to
            // stay stuck; the console it holds is released from here.
            let (done_tx, done_rx) = tokio::sync::oneshot::channel::<PtyResult<()>>();
            std::thread::spawn(move || {
                let command = format!("echo {PTY_PROBE_NONCE}");
                let result = run_one_shot(
                    &command,
                    &ctx,
                    PtySize::default(),
                    Duration::from_secs(6),
                    &killer,
                    &master,
                )
                .and_then(|(output, code)| {
                    if code == 0 && output.contains(PTY_PROBE_NONCE) {
                        Ok(())
                    } else {
                        Err(format!(
                            "the console never ran the probe command (exit code {code})"
                        ))
                    }
                });
                let _ = done_tx.send(result);
            });
            match tokio::time::timeout(Duration::from_secs(12), done_rx).await {
                Ok(Ok(Ok(()))) => {
                    clear_pty_marker();
                    true
                }
                // Failure (reported, dead thread, or wedged): kill the child and
                // release the console off this task — the probe thread may be
                // stuck mid-read and cannot do it. The release is detached
                // because a wedged console's close blocks for minutes to hours;
                // the recorded marker is what keeps the attempt from repeating.
                other => {
                    if let Some(mut killer) =
                        killer_slot.lock().unwrap_or_else(|e| e.into_inner()).take()
                    {
                        let _ = killer.kill();
                    }
                    release_master_detached(&master_slot);
                    let reason = match other {
                        Ok(Ok(Err(reason))) => reason,
                        Ok(Err(_)) => "the probe thread ended without a verdict".to_string(),
                        Err(_) => "the probe did not finish (console wedged)".to_string(),
                        Ok(Ok(Ok(()))) => unreachable!("handled above"),
                    };
                    record_pty_unavailable(&reason);
                    false
                }
            }
        })
        .await
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
        // Host capability first: creating a console on a host where ConPTY
        // children cannot start leaves a spinning `conhost.exe` behind, so
        // the tool answers with the real reason instead of a bare timeout.
        if !host_pty_supported(ctx).await {
            return Ok(err_output(
                "pty_shell is unavailable on this host: a ConPTY child process could not start \
                 (the console never ran the probe command; the failure matches Windows status \
                 0xC0000142). Use the `shell` tool instead, or run the command without a \
                 terminal. The verdict is recorded in ~/.wavecode/pty-unavailable and retried \
                 automatically after 24 h; delete that file to probe again now. Note that \
                 releasing a wedged console can take minutes, so a leftover \
                 `conhost.exe --headless` may spin until it finishes.",
            ));
        }
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
            // The killer and console stay outside the moved entry so the
            // timeout arm can unblock the reader thread (which holds the
            // session lock) and release the pseudoconsole.
            let killer = entry.killer.clone();
            let master = entry.master.clone();
            let label = command.clone();
            let name = name.clone();
            let run =
                tokio::task::spawn_blocking(move || interact(&entry, &command, size_opt, timeout));
            let (output, code) =
                match tokio::time::timeout(Duration::from_millis(timeout_ms), run).await {
                    Ok(Ok(Ok(pair))) => pair,
                    Ok(Ok(Err(reason))) => return Ok(err_output(reason)),
                    Ok(Err(_join)) => return Ok(err_output("pty session task failed")),
                    Err(_) => {
                        let _ = killer.lock().unwrap_or_else(|e| e.into_inner()).kill();
                        // The shell was killed, so the session's state is gone
                        // anyway; forgetting the entry releases its console. The
                        // abandoned reader still holds the session lock, which is
                        // why the console is reached through the shared slot.
                        {
                            let mut reg = lock_registry();
                            reg.sessions.remove(&name);
                            reg.order.retain(|n| n != &name);
                        }
                        release_master_detached(&master);
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
            let master_slot: MasterSlot = Arc::new(Mutex::new(None));
            let slot = killer_slot.clone();
            let master = master_slot.clone();
            let run = tokio::task::spawn_blocking(move || {
                run_one_shot(&command, &ctx_owned, size, timeout, &slot, &master)
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
                        if let Some(mut killer) =
                            killer_slot.lock().unwrap_or_else(|e| e.into_inner()).take()
                        {
                            let _ = killer.kill();
                        }
                        // The task is abandoned mid-read and cannot close its
                        // own console; do it here, off-thread (the release
                        // blocks while a wedged console tears down).
                        release_master_detached(&master_slot);
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

    /// Releases every live session (and its pseudoconsole) when the test
    /// ends, panics included: a console that is never closed outlives the
    /// test process as a spinning `conhost.exe` (measured).
    struct RegistryCleanup;

    impl Drop for RegistryCleanup {
        fn drop(&mut self) {
            reset_registry_for_tests();
        }
    }

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

    /// Whether the live PTY tests may run at all.
    ///
    /// Windows hosts can break ConPTY in a way that costs more than a failed
    /// test: every attempt spawns a `conhost.exe --headless` that spins until
    /// it is closed, so a plain `cargo test` would burn CPU and leak consoles
    /// from the test process (measured: one orphan per run, ~40% of a core
    /// each, for as long as the console stays open). Opt in explicitly:
    ///
    /// ```text
    /// WAVECODE_PTY_TESTS=1 cargo test -p wavecode-tools pty::
    /// ```
    ///
    /// Unix PTYs are unaffected and always run.
    fn pty_tests_enabled() -> bool {
        if cfg!(windows) {
            std::env::var("WAVECODE_PTY_TESTS").is_ok_and(|v| v == "1")
        } else {
            true
        }
    }

    /// True when a real shell echoes through a PTY within budget.
    ///
    /// The host capability probe inside `PtyShell` does the work (and caches
    /// the verdict, on disk when negative); this wrapper only adds the opt-in
    /// gate and reports why a skip happened instead of skipping silently.
    async fn pty_live() -> bool {
        static PROBE: tokio::sync::OnceCell<bool> = tokio::sync::OnceCell::const_new();
        *PROBE
            .get_or_init(|| async {
                if !pty_tests_enabled() {
                    eprintln!(
                        "pty live tests skipped: set WAVECODE_PTY_TESTS=1 to run them \
                         (a ConPTY attempt can leave a spinning conhost on Windows)"
                    );
                    return false;
                }
                let (_d, c) = ctx();
                let probed = PtyShell
                    .execute(
                        serde_json::json!({"command": format!("echo {PTY_PROBE_NONCE}"), "timeout_ms": 8000}),
                        &c,
                    )
                    .await;
                match probed {
                    Ok(out) if !out.is_error && out.content.contains(PTY_PROBE_NONCE) => true,
                    Ok(out) => {
                        eprintln!(
                            "pty live tests skipped: {}\n\
                             If a `conhost.exe --headless` is left spinning, close it with:\n  \
                             powershell -NoProfile -Command \"Get-CimInstance Win32_Process \
                             -Filter 'Name=\\\"conhost.exe\\\"' | Where-Object {{ $_.CommandLine \
                             -match '--headless' }} | Where-Object {{ -not (Get-CimInstance \
                             Win32_Process -Filter ('ProcessId=' + $_.ParentProcessId)) }} | \
                             Stop-Process -Id {{ $_.ProcessId }} -Force\"",
                            out.content
                        );
                        false
                    }
                    Err(_) => {
                        eprintln!("pty live tests skipped: the PTY probe failed");
                        false
                    }
                }
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
            .execute(serde_json::json!({"command": cmd, "timeout_ms": 1500}), &c)
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
        let _cleanup = RegistryCleanup;
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
        let _cleanup = RegistryCleanup;
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
        let _cleanup = RegistryCleanup;
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
        let _cleanup = RegistryCleanup;
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
