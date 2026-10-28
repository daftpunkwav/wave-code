/*!
 * @file RuntimeBase
 * @description OS runtime primitives: channels, interruption, limits, and
 *   the shared calendar-date and command-shell resolution.
 *
 * Responsibilities:
 * - Centralize channel capacities and timeout constants.
 * - Provide the cooperative interrupt handle shared across tasks.
 * - Document truncation budgets for event payloads.
 * - Render the calendar date once for every layer that needs it (prompt
 *   assembly and the loop's midnight-rollover notice).
 * - Resolve the platform command-string shell once for every layer that
 *   spawns one (shell tool, PTY shell, hooks, jobs).
 * - Own the process-tree kill every spawn path shares, plus the
 *   off-worker variant that keeps a blocking tree kill (Windows) off a
 *   runtime worker.
 * - Own the crash-safe file-replace primitive shared by the durable stores.
 *
 * This module must not depend on: any other workspace crate.
 */

//! Runtime base primitives with centralized limits.
//!
//! Scattered magic numbers are a maintenance hazard, so every capacity,
//! timeout, and truncation budget lives here with its rationale.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub mod console_codec;

/// Capacity of the main event channel per session.
pub const EVENT_CHANNEL_CAP: usize = 256;
/// Capacity of the control channel for interrupts and approvals.
pub const CONTROL_CHANNEL_CAP: usize = 32;
/// Maximum queued submissions per session; overflow is an explicit error.
pub const PENDING_QUEUE_CAP: usize = 64;
/// Grace period for draining in-flight work on shutdown.
pub const SHUTDOWN_DRAIN: Duration = Duration::from_secs(2);
/// Poll interval while a run parks on an approval decision.
pub const APPROVAL_POLL: Duration = Duration::from_millis(25);
/// Default timeout for lifecycle hook commands.
pub const HOOK_DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
/// Character budget for event text payloads.
pub const EVENT_TEXT_TRUNCATION: usize = 2000;
/// Character budget for approval detail strings; sized for the
/// multi-line diff a file-write approval carries. Mirrored by the
/// sandbox crate's `DETAIL_MAX_CHARS` (the producing side), pinned
/// byte-equal by the composition root's policy-adapter test — no
/// dependency edge exists between the crates.
pub const APPROVAL_DETAIL_TRUNCATION: usize = 2000;

/// Cooperative interrupt handle shared across spawned tasks.
///
/// Interrupts are delivered at safe points only: tasks poll
/// [`InterruptHandle::is_triggered`] between units of work instead of being
/// cancelled mid-operation, so no half-written state is left behind.
#[derive(Debug, Clone, Default)]
pub struct InterruptHandle {
    flag: Arc<AtomicBool>,
}

impl InterruptHandle {
    /// Create an untriggered handle.
    pub fn new() -> Self {
        Self::default()
    }

    /// Signal interruption; safe to call from any thread.
    pub fn trigger(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    /// True after [`InterruptHandle::trigger`] until [`InterruptHandle::reset`].
    pub fn is_triggered(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Clear a previous trigger, e.g. when a new run starts.
    pub fn reset(&self) {
        self.flag.store(false, Ordering::SeqCst);
    }
}

/// Truncate text to a character budget, appending an ellipsis on cut.
///
/// Operates on `char` boundaries so truncation never splits UTF-8.
/// A zero budget yields an empty string rather than a bare ellipsis.
pub fn truncate(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let kept: String = text.chars().take(max_chars).collect();
    format!("{kept}...")
}

/// Pick the shell program and its "run a command string" flag.
///
/// One resolution for every layer that spawns a command string (the shell
/// tool, the PTY shell, lifecycle hooks, background jobs), so the
/// `WAVECODE_SHELL` override can never be honored by one spawn path and
/// silently ignored by another.
///
/// Platform default: `cmd /C` on Windows, `sh -c` elsewhere. Setting
/// `WAVECODE_SHELL` overrides the program (the value is the program path);
/// the flag style is then a heuristic — values containing `cmd` use `/C`,
/// everything else uses `-c`. That covers common names (cmd, powershell,
/// bash, zsh) but may guess wrong for unusual ones; keep it simple and
/// switch to explicit configuration only when a real shell demands it.
pub fn shell_invocation() -> (String, &'static str) {
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

/// Build the command that runs `command` as a command string.
///
/// One place turns the shared resolution into a spawn, so every
/// command-string path picks the same program and the same flag style.
/// The caller adds what its own path needs: the standard streams, the
/// working directory, the process-group setup.
pub fn shell_command(command: &str) -> tokio::process::Command {
    let (program, flag) = shell_invocation();
    let mut cmd = tokio::process::Command::new(program);
    cmd.arg(flag).arg(command);
    cmd
}

/// Windows `CREATE_NEW_PROCESS_GROUP`. OR'd with flags already set on `cmd`
/// (tokio and `std` both OR repeated `creation_flags` calls), so it composes
/// with `CREATE_SUSPENDED`.
#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// Windows `CREATE_NO_WINDOW`. `taskkill` is a console subsystem binary;
/// without this flag a tree kill flashes a console window.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Mark `cmd` as its own process-group leader, the precondition of [`kill_tree`].
///
/// Unix: `process_group(0)`. Windows: `CREATE_NEW_PROCESS_GROUP`. Every spawn
/// that will later be tree-killed goes through here, so the flag cannot drift
/// from the kill helper.
pub fn lead_process_group(cmd: &mut std::process::Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    #[cfg(not(any(unix, windows)))]
    let _ = cmd;
}

/// Kill a spawned process tree by leader pid (best effort).
///
/// Precondition: the caller marked the spawn with [`lead_process_group`]
/// (Unix `Command::process_group(0)`; Windows `CREATE_NEW_PROCESS_GROUP`),
/// so the Unix branch's group signal cannot land outside that spawn. The
/// pid may have exited already — `killpg` reports ESRCH and `taskkill`
/// fails — and both degrade to no-ops. The caller still reaps the direct
/// child itself (kill/await on its handle), and it must reap the child
/// only after this kill has landed: the reap releases the pid, and a kill
/// that starts afterwards can hit a process the system already handed
/// that pid to.
///
/// On Windows this waits until `taskkill` exits, so the tree is gone when
/// the call returns. That wait is hundreds of milliseconds (measured at
/// roughly 400 ms on an idle host), so an async caller must not call this
/// directly: it would park a runtime worker for the whole wait and stall
/// every other task scheduled on it. Use [`kill_tree_blocking`] from an
/// async caller. A caller that needs the kill to have landed before it
/// continues — a spawn path that reaps the child right after — calls this
/// directly and accepts the wait.
pub fn kill_tree(pid: u32) {
    #[cfg(unix)]
    {
        // SAFETY: killpg with SIGKILL takes no callbacks and touches no
        // Rust state; ESRCH (already exited) and EPERM need no handling.
        // nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
        unsafe {
            libc::killpg(pid as libc::pid_t, libc::SIGKILL);
        }
    }
    #[cfg(windows)]
    {
        // Windows has no group-kill primitive here; `taskkill /T /F` is
        // the tree-kill analogue (the sandbox backend's kill-on-close Job
        // Object is the stronger confinement-side mechanism).
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new(taskkill_program())
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .status();
    }
    #[cfg(not(any(unix, windows)))]
    let _ = pid;
}

/// Kill a spawned process tree from an async task, off the runtime worker.
///
/// [`kill_tree`] blocks for the whole `taskkill` wait on Windows, so
/// running it on a runtime worker parks that worker for hundreds of
/// milliseconds and stalls every other task scheduled on it. This
/// helper runs the same kill on the blocking pool instead. The caller
/// waits for it, so the "tree kill first, then the direct child's own
/// kill" order the spawn paths rely on is preserved.
pub async fn kill_tree_blocking(pid: u32) {
    on_blocking_pool(move || kill_tree(pid)).await;
}

/// Run `work` on the tokio blocking pool and wait for it to finish.
async fn on_blocking_pool<F: FnOnce() + Send + 'static>(work: F) {
    // The join result carries only a panic, and the tree kill is best
    // effort by contract, so a failed task must not fail the caller.
    let _ = tokio::task::spawn_blocking(work).await;
}

/// Liveness probe for a pid the kill paths captured earlier.
///
/// A tree kill aimed at a pid whose owner already exited can hit an
/// unrelated process the OS recycled the pid onto; probing first
/// narrows that window to the gap between probe and kill (a fully
/// handle-based design needs process identities, not pids, and the
/// spawn paths have only pids by the time a cancel arrives).
#[cfg(windows)]
pub fn pid_alive(pid: u32) -> bool {
    // SAFETY: OpenProcess/GetExitCodeProcess take a pid and an out
    // pointer and close nothing; the returned handle is closed on every
    // path below.
    // nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
    unsafe {
        let process = windows_sys::Win32::System::Threading::OpenProcess(
            windows_sys::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid,
        );
        if process.is_null() {
            return false;
        }
        let mut exit_code: u32 = 0;
        let alive = windows_sys::Win32::System::Threading::GetExitCodeProcess(
            process,
            &mut exit_code,
        ) != 0
            // STILL_ACTIVE (0x103) is the sentinel GetExitCodeProcess
            // reports while the process keeps running.
            && exit_code as i32 == windows_sys::Win32::Foundation::STILL_ACTIVE;
        windows_sys::Win32::Foundation::CloseHandle(process);
        alive
    }
}

/// Liveness probe for a pid the kill paths captured earlier (see the
/// Windows variant for the rationale).
#[cfg(unix)]
pub fn pid_alive(pid: u32) -> bool {
    // SAFETY: kill with signal 0 touches no Rust state; a 0 or an EPERM
    // both mean the process exists.
    // nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
    unsafe {
        let result = libc::kill(pid as libc::pid_t, 0);
        result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

#[cfg(not(any(unix, windows)))]
pub fn pid_alive(_pid: u32) -> bool {
    false
}

/// A Job Object holding one spawned child for lifetime control
/// (Windows): `terminate` ends the child and everything it spawned
/// with one kernel call, replacing the `taskkill` process spawn (and
/// its ~0.5s tree wait) on every cancel path, and the kill-on-close
/// flag makes an abandoned job clean its membership up at process
/// exit. `None` at the assign sites degrades to the [`kill_tree`]
/// path, so a failed assignment costs latency, not correctness.
#[cfg(windows)]
pub struct ProcessTreeJob {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

// SAFETY: the raw handle is never dereferenced in Rust — every use is a
// kernel call taking it by value — and access goes through the shared
// `Arc` the cancel paths hold, so sending and sharing it carries no
// data race.
// nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
#[cfg(windows)]
unsafe impl Send for ProcessTreeJob {}
// SAFETY: see the Send impl.
// nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
#[cfg(windows)]
unsafe impl Sync for ProcessTreeJob {}

#[cfg(windows)]
impl ProcessTreeJob {
    /// Create a kill-on-close job and assign the spawned child to it.
    ///
    /// Assignment must happen before the child spawns descendants the
    /// cancel wants covered (a process joins with everything it has
    /// already spawned only when the job is created broken-away-safe —
    /// this plain assignment covers the child and its future spawns).
    pub fn assign(child: &tokio::process::Child) -> Option<Self> {
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };
        let process = child.raw_handle()?;
        // SAFETY: the job is created unnamed; the limit struct is
        // zeroed and filled before the sized setter call; the child
        // handle is a live process handle from the spawn.
        // nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return None;
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let set = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if set == 0 || AssignProcessToJobObject(job, process) == 0 {
                windows_sys::Win32::Foundation::CloseHandle(job);
                return None;
            }
            Some(Self { handle: job })
        }
    }

    /// Terminate every process in the job (the child and its spawn
    /// tree). Best effort: a failure is invisible here by contract.
    pub fn terminate(&self) {
        // SAFETY: the handle is a live job handle owned by this value;
        // the exit code has no consumer.
        // nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(self.handle, 1);
        }
    }
}

#[cfg(windows)]
impl Drop for ProcessTreeJob {
    fn drop(&mut self) {
        // SAFETY: this value owns the handle and drops once.
        // nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

/// Absolute path to the Windows tree-kill utility.
///
/// Resolved against the system directory (`%SystemRoot%\System32`) instead
/// of a bare name on purpose: `CreateProcess` searches the *current
/// directory* before the system directories, so a bare `taskkill` would
/// execute a model-planted `taskkill.exe` sitting in the working directory
/// with this process' own privileges — outside every sandbox that only
/// confines spawned children. The bare-name fallback below is unreachable
/// on a healthy Windows host (`SystemRoot` is always set) and exists only
/// so the kill still fires on a broken one rather than leaving orphaned
/// trees behind.
#[cfg(windows)]
pub fn taskkill_program() -> std::path::PathBuf {
    if let Some(system_root) = std::env::var_os("SystemRoot") {
        let path = std::path::PathBuf::from(system_root)
            .join("System32")
            .join("taskkill.exe");
        if path.is_file() {
            return path;
        }
    }
    std::path::PathBuf::from("taskkill")
}

/// Atomic file replace: write `contents` to a unique sibling staging file,
/// then rename it over `path`, so a crash mid-write leaves the target with
/// either the old or the new content — never a half-written mix.
///
/// The staging name carries the process id plus a per-process sequence
/// number: a fixed staging name lets two writers clobber each other's bytes
/// mid-write and rename half a file into place. A failed rename removes the
/// staging file best-effort (no litter; the error still propagates). Sync
/// `std::fs` only — stores needing stronger durability (fsync before the
/// rename) keep their own write path on purpose.
pub fn atomic_write(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    let staging = staging_path_for(path);
    std::fs::write(&staging, contents)?;
    if let Err(e) = std::fs::rename(&staging, path) {
        let _ = std::fs::remove_file(&staging);
        return Err(e);
    }
    Ok(())
}

/// Unique sibling staging name for an atomic replace (shared by
/// [`atomic_write`] and [`atomic_write_private`]): process id plus a
/// per-process sequence number.
fn staging_path_for(path: &std::path::Path) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    match path.extension() {
        Some(ext) => path.with_extension(format!(
            "{}.staging-{}-{}",
            ext.to_string_lossy(),
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        )),
        None => path.with_extension(format!(
            "staging-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        )),
    }
}

/// Owner-only file mode for local private data (Unix `0o600`).
///
/// Everything a session writes under the home directory (`~/.wavecode`:
/// journals, input history, plans, goals, schedules, memories, the spill
/// side-store and its manifest, grants, and other session data) carries
/// user content, so on multi-user Unix systems it must not be group/world
/// readable. Windows needs no equivalent: default profile ACLs already
/// scope files to the owning user, so every helper here is a plain write
/// there.
#[cfg(unix)]
const PRIVATE_MODE: u32 = 0o600;

/// Tighten an open file to owner-only (Unix only; the call is a no-op on
/// Windows where the default ACL already scopes to the user).
///
/// Re-tightening on every write is deliberate: files created before this
/// policy existed (or by a looser writer) are repaired the next time they
/// are written instead of staying readable by other local users forever.
#[cfg(unix)]
fn tighten_private(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(PRIVATE_MODE))
}

/// Write `contents` to `path` (create or truncate) as an owner-only file.
///
/// The private-data sibling of [`std::fs::write`] for files that carry user
/// content: conversation journals, spill payloads, credential stores. The
/// write and the mode are one operation, so a crash cannot leave a newly
/// created file world-readable.
pub fn write_private(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(PRIVATE_MODE)
            .open(path)?;
        tighten_private(&file)?;
        use std::io::Write;
        file.write_all(contents)
    }
    #[cfg(not(unix))]
    std::fs::write(path, contents)
}

/// [`atomic_write`] for private data: the staging file is created
/// owner-only before the rename, so the replaced file never carries a
/// looser mode (rename preserves the staging file's permissions).
pub fn atomic_write_private(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    let staging = staging_path_for(path);
    if let Err(e) = write_private(&staging, contents) {
        let _ = std::fs::remove_file(&staging);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&staging, path) {
        let _ = std::fs::remove_file(&staging);
        return Err(e);
    }
    Ok(())
}

/// Open `path` for appending, creating it owner-only when missing.
///
/// The private-data form of `OpenOptions::create + append` for append-only
/// local logs (history journals, input history): every write also re-tightens
/// the mode, so a pre-policy `0o644` file is repaired on its next append.
pub fn open_append_private(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(PRIVATE_MODE)
            .open(path)?;
        tighten_private(&file)?;
        Ok(file)
    }
    #[cfg(not(unix))]
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

/// Current unix time in whole seconds; 0 when the clock is before the
/// epoch (a machine with an unset or badly set RTC).
///
/// One clock read for every layer that stamps wall-clock time (spill
/// manifests, grant provenance, metric samples, the session index), so
/// the saturate-to-zero convention cannot drift between stores. Stamp
/// semantics are provenance-only by design: nothing orders work by these
/// seconds. Callers needing sub-second resolution use [`now_secs_f64`].
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// [`now_secs`] with sub-second precision, for callers measuring elapsed
/// time rather than stamping provenance (token-bucket refills).
pub fn now_secs_f64() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Weekday names indexed by `days_since_epoch % 7` with 1970-01-01 =
/// Thursday (index 4).
const WEEKDAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

/// Render `now` as `YYYY-MM-DD (Weekday)` in UTC.
///
/// One rendering for every layer that shows a date (the prompt's environment
/// section and the loop's midnight-rollover notice), so the two can never
/// disagree. UTC-vs-local skew only matters around midnight and is accepted
/// in exchange for staying dependency-free.
pub fn format_date(now: std::time::SystemTime) -> String {
    // Whole days since the epoch, rounding toward negative infinity so a
    // pre-epoch clock (a machine with an unset or badly set RTC) renders the
    // real calendar day instead of silently clamping to 1970-01-01.
    let days: i64 = match now.duration_since(std::time::UNIX_EPOCH) {
        Ok(since) => (since.as_secs() / 86_400) as i64,
        Err(before) => {
            let secs = before.duration().as_secs();
            // Ceil, then negate: 1 s before the epoch is 1969-12-31, not -0.
            -(secs.div_ceil(86_400) as i64)
        }
    };
    let (year, month, day) = civil_from_days(days);
    let weekday = WEEKDAYS[(days + 4).rem_euclid(7) as usize];
    format!("{year:04}-{month:02}-{day:02} ({weekday})")
}

/// Render epoch seconds as `YYYY-MM-DD HH:MM UTC`.
///
/// One rendering for every layer that shows a wall-clock instant (persisted
/// tool-result headers), sharing the civil-date math with [`format_date`].
/// UTC-only matches [`format_date`]: local-offset rendering would need a
/// timezone database, which the dependency-free date formatting here
/// deliberately avoids.
pub fn format_timestamp(secs: u64) -> String {
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    let tod = secs % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        tod / 3_600,
        (tod % 3_600) / 60
    )
}

/// Convert days since 1970-01-01 to a proleptic Gregorian (year, month,
/// day); Howard Hinnant's civil-from-days algorithm, valid for negative
/// day counts too.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (year + i64::from(month <= 2), month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

    /// The blocking-pool runner must keep the runtime worker free: the
    /// work runs on the blocking pool, so a task scheduled on the same
    /// worker keeps making progress while it runs. A worker that ran
    /// the work inline would starve every other task on it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn blocking_pool_work_leaves_the_runtime_worker_free() {
        let progress = Arc::new(AtomicUsize::new(0));
        let counter = progress.clone();
        // The ticker shares the single worker with the task below.
        let ticker = tokio::spawn(async move {
            for _ in 0..400 {
                tokio::time::sleep(Duration::from_millis(5)).await;
                counter.fetch_add(1, Ordering::Relaxed);
            }
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        let ran_on = Arc::new(Mutex::new(None));
        let (before, worker) = tokio::spawn({
            let progress = progress.clone();
            let ran_on = ran_on.clone();
            async move {
                let worker = std::thread::current().id();
                // The snapshot is taken on the worker, immediately
                // before the blocking work starts: no await point sits
                // between the read and the park, so the ticker cannot
                // slip a tick into the window.
                let before = progress.load(Ordering::Relaxed);
                on_blocking_pool({
                    let ran_on = ran_on.clone();
                    move || {
                        *ran_on.lock().unwrap_or_else(|e| e.into_inner()) =
                            Some(std::thread::current().id());
                        std::thread::sleep(Duration::from_millis(200));
                    }
                })
                .await;
                (before, worker)
            }
        })
        .await
        .expect("the worker task finished");
        let after = progress.load(Ordering::Relaxed);
        ticker.abort();
        assert_ne!(
            ran_on.lock().unwrap_or_else(|e| e.into_inner()).as_ref(),
            Some(&worker),
            "the blocking work ran on the runtime worker"
        );
        assert!(
            after > before,
            "the runtime worker was parked while the blocking work ran"
        );
    }

    /// The blocking-pool runner waits for its work: the spawn paths rely
    /// on "tree kill first, then the direct child's own kill", so a
    /// variant that returned before the kill landed would let the shell
    /// die first and orphan the descendants that inherited its pipes.
    #[tokio::test]
    async fn blocking_pool_work_finishes_before_the_runner_returns() {
        let finished = Arc::new(AtomicBool::new(false));
        let flag = finished.clone();
        on_blocking_pool(move || {
            std::thread::sleep(Duration::from_millis(50));
            flag.store(true, Ordering::SeqCst);
        })
        .await;
        assert!(
            finished.load(Ordering::SeqCst),
            "the runner returned before the work had finished"
        );
    }

    #[test]
    fn interrupt_handle_triggers_and_resets() {
        let handle = InterruptHandle::new();
        assert!(!handle.is_triggered());
        handle.trigger();
        assert!(handle.is_triggered());
        // Clones share the same flag.
        assert!(handle.clone().is_triggered());
        handle.reset();
        assert!(!handle.is_triggered());
    }

    #[test]
    fn truncation_keeps_short_text_and_cuts_long_text() {
        assert_eq!(truncate("abc", 10), "abc");
        let cut = truncate("abcdef", 3);
        assert_eq!(cut, "abc...");
    }

    #[test]
    fn truncation_zero_budget_returns_empty() {
        assert_eq!(truncate("abcdef", 0), "");
        assert_eq!(truncate("", 0), "");
    }

    #[test]
    fn truncation_cuts_on_char_boundaries() {
        assert_eq!(
            truncate("\u{65e5}\u{672c}\u{8a9e}", 2),
            "\u{65e5}\u{672c}..."
        );
        assert_eq!(truncate("a\u{1f642}b", 2), "a\u{1f642}...");
    }

    /// Serializes tests that mutate the process-global `WAVECODE_SHELL`
    /// variable; parallel env mutation would race between threads.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn shell_invocation_defaults_to_the_platform_shell() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var("WAVECODE_SHELL").ok();
        // nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
        unsafe {
            std::env::remove_var("WAVECODE_SHELL");
        }
        let (program, flag) = shell_invocation();
        // nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
        unsafe {
            if let Some(v) = prior {
                std::env::set_var("WAVECODE_SHELL", v);
            }
        }
        if cfg!(windows) {
            assert_eq!((program.as_str(), flag), ("cmd", "/C"));
        } else {
            assert_eq!((program.as_str(), flag), ("sh", "-c"));
        }
    }

    #[test]
    fn shell_invocation_override_picks_flag_style_by_name() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var("WAVECODE_SHELL").ok();
        // A cmd-like name keeps the Windows command-string flag.
        // nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
        unsafe {
            std::env::set_var("WAVECODE_SHELL", r"C:\tools\my-cmd.exe");
        }
        let (program, flag) = shell_invocation();
        assert_eq!(program, r"C:\tools\my-cmd.exe");
        assert_eq!(flag, "/C");
        // Any other name gets the Unix command-string flag.
        // nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
        unsafe {
            std::env::set_var("WAVECODE_SHELL", "/usr/bin/zsh");
        }
        let (program, flag) = shell_invocation();
        assert_eq!(program, "/usr/bin/zsh");
        assert_eq!(flag, "-c");
        // nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
        unsafe {
            std::env::remove_var("WAVECODE_SHELL");
            if let Some(v) = prior {
                std::env::set_var("WAVECODE_SHELL", v);
            }
        }
    }

    #[test]
    fn atomic_write_replaces_content_and_leaves_no_staging_litter() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("store.json");
        atomic_write(&path, b"first").expect("first write");
        assert_eq!(std::fs::read(&path).unwrap(), b"first");
        // An overwrite swaps wholesale: the destination always holds the
        // old or the new content, and the staging file is gone once the
        // rename lands.
        atomic_write(&path, b"second").expect("overwrite");
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        let litter: Vec<String> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".staging-"))
            .collect();
        assert!(
            litter.is_empty(),
            "staging files must not linger: {litter:?}"
        );
    }

    /// A failed rename must clean up its staging file instead of littering
    /// the store directory: callers fail loudly with the destination left
    /// untouched.
    #[test]
    fn failed_atomic_write_removes_its_staging_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("store.json");
        // A directory at the destination makes the final rename fail on
        // every platform (a file cannot replace a directory).
        std::fs::create_dir(&path).expect("blocker dir");
        assert!(
            atomic_write(&path, b"new").is_err(),
            "renaming onto a directory must fail"
        );
        let litter: Vec<String> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".staging-"))
            .collect();
        assert!(
            litter.is_empty(),
            "the staging file must be removed: {litter:?}"
        );
    }

    /// Private writes land the content and create the file owner-only;
    /// an existing looser mode is tightened on rewrite.
    #[test]
    fn private_writes_create_and_tighten_owner_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.jsonl");
        write_private(&path, b"first").expect("first write");
        assert_eq!(std::fs::read(&path).unwrap(), b"first");
        write_private(&path, b"second").expect("overwrite truncates");
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(
                mode & 0o777,
                0o600,
                "private file stays owner-only: {mode:o}"
            );
            // A pre-policy world-readable file is repaired on the next write.
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            write_private(&path, b"third").unwrap();
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "loose mode tightened: {mode:o}");
        }
    }

    /// The private atomic replace carries the owner-only mode onto the
    /// destination and leaves no staging litter behind.
    #[test]
    fn atomic_write_private_lands_owner_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("index.json");
        atomic_write_private(&path, b"one").expect("first write");
        atomic_write_private(&path, b"two").expect("replace");
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(
                mode & 0o777,
                0o600,
                "replaced file stays owner-only: {mode:o}"
            );
        }
        let litter: Vec<String> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".staging-"))
            .collect();
        assert!(litter.is_empty(), "no staging litter: {litter:?}");
    }

    /// Private appends preserve prior content and keep the file owner-only,
    /// including repairing a pre-policy loose mode.
    #[test]
    fn open_append_private_preserves_content_and_tightens() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("history.jsonl");
        {
            let mut file = open_append_private(&path).expect("create");
            use std::io::Write;
            writeln!(file, "one").unwrap();
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        {
            let mut file = open_append_private(&path).expect("append");
            use std::io::Write;
            writeln!(file, "two").unwrap();
        }
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(raw, "one\ntwo\n", "append never truncates");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "append tightened the mode: {mode:o}");
        }
    }

    #[test]
    fn now_secs_reads_a_plausible_clock() {
        // 2023-01-01T00:00:00Z; anything older means a broken RTC, which
        // the saturate-to-zero contract would surface here.
        assert!(now_secs() > 1_672_531_200);
        assert!(now_secs_f64() > 1_672_531_200.0);
    }

    /// The tree-kill utility resolves to the absolute system path, never a
    /// bare name: CreateProcess would otherwise search the current
    /// directory (where a model can plant files) before the system
    /// directories and execute a planted `taskkill.exe` with this
    /// process' own privileges.
    #[cfg(windows)]
    #[test]
    fn taskkill_resolves_to_the_system_directory() {
        let program = taskkill_program();
        if !program.is_absolute() {
            // Only reachable when SystemRoot is unset (broken host).
            eprintln!("SystemRoot unset; skipping the taskkill resolution test");
            return;
        }
        let system_root = std::env::var_os("SystemRoot").expect("checked above");
        let expected = std::path::PathBuf::from(system_root)
            .join("System32")
            .join("taskkill.exe");
        assert_eq!(program, expected, "tree kill must anchor at System32");
        assert!(
            program.is_file(),
            "resolved taskkill must exist: {program:?}"
        );
    }

    #[test]
    fn date_renders_known_days() {
        let days = |n: u64| std::time::UNIX_EPOCH + Duration::from_secs(n * 86_400);
        // 1970-01-01 was a Thursday.
        assert_eq!(format_date(days(0)), "1970-01-01 (Thursday)");
        // 2026-09-19 was a Saturday.
        assert_eq!(format_date(days(20_715)), "2026-09-19 (Saturday)");
        // 2000-02-29 (leap day, a Tuesday).
        assert_eq!(format_date(days(11_016)), "2000-02-29 (Tuesday)");
        // Year boundaries stay contiguous.
        assert_eq!(format_date(days(364)), "1970-12-31 (Thursday)");
        assert_eq!(format_date(days(365)), "1971-01-01 (Friday)");
    }

    #[test]
    fn date_before_the_epoch_stays_correct() {
        // 1969-12-31 was a Wednesday; a broken RTC must not render 1970.
        let before = std::time::UNIX_EPOCH - Duration::from_secs(86_400);
        assert_eq!(format_date(before), "1969-12-31 (Wednesday)");
        // One second before the epoch is still the previous day.
        let just_before = std::time::UNIX_EPOCH - Duration::from_secs(1);
        assert_eq!(format_date(just_before), "1969-12-31 (Wednesday)");
        // Exactly the epoch boundary.
        assert_eq!(format_date(std::time::UNIX_EPOCH), "1970-01-01 (Thursday)");
    }

    #[test]
    fn timestamp_renders_known_instants() {
        // 1970-01-01T00:00:00Z.
        assert_eq!(format_timestamp(0), "1970-01-01 00:00 UTC");
        // 2026-09-19T18:03:00Z: minutes and hours render padded.
        assert_eq!(
            format_timestamp(20_715 * 86_400 + 18 * 3_600 + 3 * 60),
            "2026-09-19 18:03 UTC"
        );
        // End of day rolls into the next day's date.
        assert_eq!(
            format_timestamp(20_715 * 86_400 + 86_399),
            "2026-09-19 23:59 UTC"
        );
        assert_eq!(format_timestamp(20_716 * 86_400), "2026-09-20 00:00 UTC");
    }
}
