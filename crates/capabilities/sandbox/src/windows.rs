/*! @file WindowsJobBackend
 *  @description Partial Windows confinement via Win32 Job Objects.
 *
 *  Responsibilities:
 *  - Probe that a configured Job Object can be created on this host.
 *  - Arm one spawn so the child is created suspended and assigned to a
 *    kill-on-close Job Object before its first instruction runs.
 *  - Enforce process-tree lifetime control (kill on job close) plus an
 *    active-process cap per spawn.
 *  - Record what stays unenforced (filesystem boundary, network policy) and
 *    keep refusing for anything beyond the Job-Object scope.
 *
 *  This module must not depend on: tools, transport, frontend crates.
 */

use super::os::{ArmedSpawn, ConfinementProfile, EnforcementLevel, SandboxBackend, SandboxError};

#[cfg(windows)]
use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;

/// What stays unenforced on Windows even with the Job-Object backend active:
/// the Job Object primitive has no filesystem or network surface at all (no
/// per-path ACLs are lowered and no filter driver is attached), and the
/// integrity-token / AppContainer machinery for a *full* isolation boundary
/// is a much larger FFI scope that is deliberately not implemented. Any
/// requested confinement that needs those guarantees fails closed instead of
/// running unconfined (the caller turns a backend failure into a hard error).
pub const WINDOWS_UNAVAILABLE_REASON: &str = "no full-isolation Windows backend: the Job-Object backend (windows::WindowsJobBackend) enforces only process-tree lifetime control and process-count limits — no filesystem write boundary and no network policy; integrity-level / AppContainer isolation is not implemented, so confinement requests beyond that scope fail closed";

/// Short-form gap summary the job backend appends to its status line (see
/// [`SandboxBackend::status_appendix`]): the plain
/// `job (enforcement: Partial, available)` rendering would otherwise
/// overstate what the platform boundary holds. Pinned against
/// [`WINDOWS_UNAVAILABLE_REASON`] (the long-form reason) by a test, so
/// the two disclosures cannot drift apart.
pub const JOB_STATUS_GAP: &str = "process-tree lifetime control and process-count limits only: no filesystem write boundary, no network policy";

/// Hard cap on concurrently live processes inside one confined spawn's job:
/// bounds fork-bomb style runaways; the job rejects further members with a
/// spawn error inside the child instead of growing without bound.
#[cfg(windows)]
const MAX_PROCESSES_PER_JOB: u32 = 128;

/// Watcher cadence while a spawn is pending (the armed child waits suspended,
/// so discovery latency directly adds to command start-up latency).
#[cfg(windows)]
const POLL_PENDING_MS: u64 = 5;
/// Watcher cadence while idle (no pending spawn): just cheap registry checks.
#[cfg(windows)]
const POLL_IDLE_MS: u64 = 250;
/// A pending job older than this is reaped: the caller never spawned the
/// armed command (or the watcher missed the child), and the job is dropped —
/// an assigned-but-unresumed child cannot exist past this point without the
/// caller's own timeout killing it (it stays suspended the whole time).
#[cfg(windows)]
const PENDING_TTL_MS: u64 = 10_000;

/// Windows Job-Object backend: arms the spawn so the child is created
/// **suspended** (`CREATE_SUSPENDED`), assigned to a per-spawn Job Object
/// carrying `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` plus an active-process cap,
/// and only then resumed — so the process never executes a single
/// instruction outside the job (the fail-closed contract).
///
/// Why the watcher: stable Rust exposes no pre-exec hook and no
/// `PROC_THREAD_ATTRIBUTE_LIST` on `std::process::Command`, and job
/// assignment (`AssignProcessToJobObject`) strictly needs the child handle
/// that only exists after spawn. `spawn_confined` therefore registers a
/// pending job and returns an [`ArmedSpawn`](crate::os::ArmedSpawn) that
/// owns the command. [`ArmedSpawn::spawn`](crate::os::ArmedSpawn::spawn)
/// creates the suspended child and commits that exact pid; the watcher
/// assigns the job and resumes the child, or **terminates** it when
/// assignment fails. Dropping the guard without spawning cancels that
/// pending job. Pairing is by arm id, so children spawned by other code
/// (hooks, background jobs) are never confined, and two concurrent arms
/// cannot swap jobs.
///
/// What IS enforced (honest scope, `EnforcementLevel::Partial`):
/// - Process-tree lifetime control: every descendant joins the job
///   (membership is inherited), nothing can break away (neither
///   `BREAKAWAY_OK` nor `SILENT_BREAKAWAY_OK` is granted), and when the job
///   handle closes — at agent exit at the latest — the whole tree dies.
///   This also removes the shell tool's "orphaned grandchildren" gap: a
///   grandchild detached by its parent stays contained and dies with the
///   agent instead of leaking.
/// - An active-process cap per spawn (`MAX_PROCESSES_PER_JOB`).
///
/// What is NOT enforced: no filesystem write boundary, no network policy —
/// `profile` is deliberately ignored (Job Objects cannot express those
/// dimensions; see [`WINDOWS_UNAVAILABLE_REASON`]). The confinement verdict
/// is Partial; a full-isolation claim would be dishonest.
///
/// Unlike bwrap / seatbelt this does NOT rewrite `cmd` — it only adds
/// `CREATE_SUSPENDED` to the creation flags, so stdio / env / `kill_on_drop`
/// configured by the caller keep working.
#[derive(Debug, Clone, Copy, Default)]
pub struct WindowsJobBackend;

impl WindowsJobBackend {
    /// Pure probe combination (hermetic seam): Windows target plus a
    /// successful configured-job round-trip.
    pub fn probe_combine(is_windows: bool, job_creation_ok: bool) -> bool {
        is_windows && job_creation_ok
    }
}

impl SandboxBackend for WindowsJobBackend {
    fn backend_name(&self) -> &'static str {
        "job"
    }

    fn enforcement(&self) -> EnforcementLevel {
        // Lifetime control and limits only: no filesystem, no network.
        EnforcementLevel::Partial
    }

    fn status_appendix(&self) -> Option<&'static str> {
        Some(JOB_STATUS_GAP)
    }

    fn is_available(&self) -> bool {
        Self::probe_combine(cfg!(windows), probe_job_object())
    }

    fn spawn_confined(
        &self,
        cmd: tokio::process::Command,
        _profile: &ConfinementProfile,
    ) -> Result<ArmedSpawn, SandboxError> {
        #[cfg(not(windows))]
        {
            let _ = cmd;
            Err(SandboxError::Unavailable {
                platform: std::env::consts::OS,
            })
        }
        #[cfg(windows)]
        {
            if !self.is_available() {
                return Err(SandboxError::Unavailable {
                    platform: "windows",
                });
            }
            let job = create_confined_job().map_err(|e| SandboxError::ConfineFailed {
                reason: format!("create job object: {e}"),
            })?;
            let id = next_pending_id();
            {
                let mut registry = crate::lock(registry());
                if !registry.watcher_started {
                    return Err(SandboxError::ConfineFailed {
                        reason: "job assignment watcher thread unavailable".to_owned(),
                    });
                }
                registry.pending.push(PendingJob {
                    id,
                    job,
                    armed_at: std::time::Instant::now(),
                });
            }
            // The child starts suspended. ArmedSpawn::spawn commits this
            // arm's id to the spawned pid; dropping the guard cancels it.
            let mut cmd = cmd;
            cmd.creation_flags(CREATE_SUSPENDED);
            Ok(ArmedSpawn::with_pairing(
                cmd,
                move |pid| commit_pending(id, pid),
                move || cancel_pending(id),
            ))
        }
    }
}

/// Pair a spawned child with the pending job registered under `id`.
#[cfg(windows)]
fn commit_pending(id: u64, child_pid: u32) {
    let mut guard = crate::lock(registry());
    let Some(pos) = guard.pending.iter().position(|pending| pending.id == id) else {
        return;
    };
    let PendingJob { job, .. } = guard.pending.remove(pos);
    guard.designated.insert(
        child_pid,
        DesignatedJob {
            job,
            committed_at: std::time::Instant::now(),
        },
    );
}

/// Drop an arm that never spawned: its job must not stay pending for a
/// later commit to claim.
#[cfg(windows)]
fn cancel_pending(id: u64) {
    let mut guard = crate::lock(registry());
    guard.pending.retain(|pending| pending.id != id);
}

#[cfg(windows)]
fn next_pending_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Probe: can this host create a fully configured Job Object? The job is
/// dropped immediately (its handle closes); enforcement happens per-spawn.
#[cfg(windows)]
fn probe_job_object() -> bool {
    create_confined_job().is_ok()
}

#[cfg(not(windows))]
fn probe_job_object() -> bool {
    false
}

/// One spawn registered by `spawn_confined` and awaiting its committed pid:
/// the configured job plus the arming timestamp (for expiry).
#[cfg(windows)]
struct PendingJob {
    id: u64,
    job: std::os::windows::io::OwnedHandle,
    armed_at: std::time::Instant,
}

/// A committed spawn: the pid the caller reported as spawned, paired with
/// its job plus the commit timestamp (for expiry when the child died before
/// the watcher ever saw it — its job then closes as a no-op).
#[cfg(windows)]
struct DesignatedJob {
    job: std::os::windows::io::OwnedHandle,
    committed_at: std::time::Instant,
}

/// Shared watcher state. `pending` holds armed spawns waiting for their
/// child's commit; `designated` holds committed pids waiting for their
/// watcher pairing. Only designated pids are ever paired with a job.
#[cfg(windows)]
struct Registry {
    pending: Vec<PendingJob>,
    designated: std::collections::HashMap<u32, DesignatedJob>,
    watcher_started: bool,
}

/// The process-global registry, starting the watcher thread exactly once.
/// A watcher-start failure is recorded (`watcher_started == false`) and
/// makes every subsequent `spawn_confined` fail closed.
#[cfg(windows)]
fn registry() -> &'static std::sync::Mutex<Registry> {
    static REGISTRY: std::sync::OnceLock<std::sync::Mutex<Registry>> = std::sync::OnceLock::new();
    let mutex = REGISTRY.get_or_init(|| {
        std::sync::Mutex::new(Registry {
            pending: Vec::new(),
            designated: std::collections::HashMap::new(),
            watcher_started: false,
        })
    });
    let mut guard = crate::lock(mutex);
    if !guard.watcher_started {
        // Leave the flag false on spawn failure: callers must refuse, never
        // run unconfined. A later call retries the start.
        if std::thread::Builder::new()
            .name("sandbox-job-watcher".into())
            .spawn(move || watcher_loop(mutex))
            .is_ok()
        {
            guard.watcher_started = true;
        }
    }
    mutex
}

/// Watcher main loop: ticks fast while a spawn is in flight — an armed
/// child waits suspended (so discovery latency adds to command start-up)
/// and a committed pid waits for its job assignment — and sleeps idle only
/// when neither queue has work. A panicking tick is caught (the loop must
/// survive: a dead watcher would leave armed children suspended forever).
#[cfg(windows)]
fn watcher_loop(registry: &'static std::sync::Mutex<Registry>) {
    let pending_sleep = std::time::Duration::from_millis(POLL_PENDING_MS);
    let idle_sleep = std::time::Duration::from_millis(POLL_IDLE_MS);
    loop {
        // One lock acquisition covers both queues: work exists while an
        // armed spawn waits for its commit OR a committed pid waits for
        // its assignment (after the commit the pending queue is empty).
        let has_work = {
            let guard = crate::lock(registry);
            !guard.pending.is_empty() || !guard.designated.is_empty()
        };
        if has_work {
            let _ =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| watcher_tick(registry)));
            std::thread::sleep(pending_sleep);
        } else {
            std::thread::sleep(idle_sleep);
        }
    }
}

/// One watcher pass: expire stale pendings and stale committed pids, then
/// assign each committed pid that is still a live direct child of this
/// process to its job (resume; terminate on any failure). Only committed
/// pids are ever paired, so a child spawned by other code in the pairing
/// window is never confined, and a committed pid whose process already died
/// (and whose pid was reused by an unrelated process) fails the direct-child
/// check and is left to expire instead of being assigned.
#[cfg(windows)]
fn watcher_tick(registry: &'static std::sync::Mutex<Registry>) {
    let now = std::time::Instant::now();
    let ttl = std::time::Duration::from_millis(PENDING_TTL_MS);
    let matched: Vec<(u32, std::os::windows::io::OwnedHandle)> = {
        let mut guard = crate::lock(registry);
        guard
            .pending
            .retain(|p| now.duration_since(p.armed_at) < ttl);
        // Expired jobs drop here: with no member processes the close is a
        // no-op (the child, if any, stays suspended for the caller's timeout).
        guard
            .designated
            .retain(|_, d| now.duration_since(d.committed_at) < ttl);
        if guard.designated.is_empty() {
            return;
        }
        let live: std::collections::HashSet<u32> = direct_child_pids().into_iter().collect();
        let pids: Vec<u32> = guard.designated.keys().copied().collect();
        let mut matched = Vec::new();
        for pid in pids {
            if live.contains(&pid)
                && let Some(d) = guard.designated.remove(&pid)
            {
                matched.push((pid, d.job));
            }
        }
        matched
    };
    for (pid, job) in matched {
        if assign_and_resume(&job, pid).is_ok() {
            spawn_reaper(job);
        }
        // On assignment failure the child was terminated (never ran
        // unconfined); dropping the empty job is a safe no-op.
    }
}

/// Keep the job handle open until every member process has exited: a
/// detached reaper waits on the job (signaled when its process count reaches
/// zero) and only then closes the handle. Until that moment
/// `KILL_ON_JOB_CLOSE` stays armed for the whole tree.
#[cfg(windows)]
fn spawn_reaper(job: std::os::windows::io::OwnedHandle) {
    use std::os::windows::io::AsRawHandle;

    let _ = std::thread::Builder::new()
        .name("sandbox-job-reaper".into())
        .spawn(move || {
            // SAFETY: the job handle is owned by this thread for its whole
            // lifetime; the wait returns once all member processes exited.
            // nosemgrep: Semgrep_rust.lang.security.unsafe-usage.unsafe-usage
            unsafe {
                windows_sys::Win32::System::Threading::WaitForSingleObject(
                    job.as_raw_handle().cast(),
                    windows_sys::Win32::System::Threading::INFINITE,
                );
            }
            drop(job);
        });
    // A failed reaper spawn leaks the handle open (never closed early) — the
    // confinement stays armed until process exit, never weakened.
}

/// Create a Job Object configured for one confined spawn: kill-on-close plus
/// an active-process cap; breakaway is deliberately NOT granted, so members
/// cannot escape the job.
#[cfg(windows)]
fn create_confined_job() -> std::io::Result<std::os::windows::io::OwnedHandle> {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::{
        CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };

    // SAFETY: no name, no security attributes — both null; the returned raw
    // handle is either null (error) or exclusively owned from here on.
    // nosemgrep: Semgrep_rust.lang.security.unsafe-usage.unsafe-usage
    let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if handle.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the job exists (created above) and the struct is zeroed and
    // fully initialized before the call; on failure ownership stays here and
    // the handle is closed before returning the error.
    // nosemgrep: Semgrep_rust.lang.security.unsafe-usage.unsafe-usage
    let configured = unsafe {
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        limits.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
        limits.BasicLimitInformation.ActiveProcessLimit = MAX_PROCESSES_PER_JOB;
        SetInformationJobObject(
            handle,
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const std::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if configured == 0 {
        let err = std::io::Error::last_os_error();
        // SAFETY: the job handle is still exclusively owned (not yet wrapped).
        // nosemgrep: Semgrep_rust.lang.security.unsafe-usage.unsafe-usage
        unsafe { CloseHandle(handle) };
        return Err(err);
    }
    // SAFETY: the raw job handle is owned exclusively and wrapped for RAII.
    // nosemgrep: Semgrep_rust.lang.security.unsafe-usage.unsafe-usage
    Ok(unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(handle) })
}

/// Assign the suspended child `pid` to `job` and resume it. Fail-closed: any
/// failure terminates the process before it ever ran a single instruction
/// outside confinement.
#[cfg(windows)]
fn assign_and_resume(job: &std::os::windows::io::OwnedHandle, pid: u32) -> std::io::Result<()> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SET_QUOTA, PROCESS_SUSPEND_RESUME, PROCESS_TERMINATE, TerminateProcess,
    };

    // SAFETY: pid came from the process snapshot as a live child; the
    // returned handle (or null) is exclusively owned by this scope.
    // nosemgrep: Semgrep_rust.lang.security.unsafe-usage.unsafe-usage
    let process = unsafe {
        OpenProcess(
            PROCESS_SET_QUOTA | PROCESS_TERMINATE | PROCESS_SUSPEND_RESUME,
            0,
            pid,
        )
    };
    if process.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the raw process handle is owned exclusively and wrapped.
    // nosemgrep: Semgrep_rust.lang.security.unsafe-usage.unsafe-usage
    let process = unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(process) };
    // SAFETY: both handles are valid and exclusively owned.
    // nosemgrep: Semgrep_rust.lang.security.unsafe-usage.unsafe-usage
    let assigned = unsafe {
        AssignProcessToJobObject(job.as_raw_handle().cast(), process.as_raw_handle().cast())
    };
    if assigned == 0 {
        let err = std::io::Error::last_os_error();
        // Fail-closed: never let the child run unconfined.
        // SAFETY: the process handle is valid and exclusively owned.
        // nosemgrep: Semgrep_rust.lang.security.unsafe-usage.unsafe-usage
        unsafe { TerminateProcess(process.as_raw_handle().cast(), 1) };
        return Err(err);
    }
    if resume_process(pid).is_err() {
        // Fail-closed: assigned but not runnable is still a dead end — kill.
        // SAFETY: the process handle is valid and exclusively owned.
        unsafe { TerminateProcess(process.as_raw_handle().cast(), 1) };
        return Err(std::io::Error::other(
            "assigned to job but could not be resumed",
        ));
    }
    Ok(())
}

/// Resume the initial thread of `pid` (the process was created suspended):
/// enumerate its threads and resume the first one.
#[cfg(windows)]
fn resume_process(pid: u32) -> std::io::Result<()> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

    // SAFETY: the snapshot handle is null or INVALID_HANDLE_VALUE on error,
    // exclusively owned otherwise and closed via RAII.
    // nosemgrep: Semgrep_rust.lang.security.unsafe-usage.unsafe-usage
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot.is_null() || snapshot == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the raw snapshot handle is owned exclusively and wrapped.
    // nosemgrep: Semgrep_rust.lang.security.unsafe-usage.unsafe-usage
    let _snapshot_guard = unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(snapshot) };
    // SAFETY: `entry.dwSize` is initialized before each API call as the
    // ToolHelp contract requires; the snapshot handle is valid throughout.
    unsafe {
        let mut entry: THREADENTRY32 = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        if Thread32First(snapshot, &mut entry) == 0 {
            return Err(std::io::Error::last_os_error());
        }
        loop {
            if entry.th32OwnerProcessID == pid {
                let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                if thread.is_null() {
                    return Err(std::io::Error::last_os_error());
                }
                // SAFETY: the raw thread handle is owned exclusively.
                let thread = std::os::windows::io::OwnedHandle::from_raw_handle(thread);
                if ResumeThread(thread.as_raw_handle().cast()) == u32::MAX {
                    return Err(std::io::Error::last_os_error());
                }
                return Ok(());
            }
            if Thread32Next(snapshot, &mut entry) == 0 {
                return Err(std::io::Error::other("no thread found for process"));
            }
            entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        }
    }
}

/// Snapshot of this process' direct children (parent pid match only — the
/// armed child is spawned by this exact process).
#[cfg(windows)]
fn direct_child_pids() -> Vec<u32> {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;

    // SAFETY: snapshot ownership as in `resume_process`.
    // nosemgrep: Semgrep_rust.lang.security.unsafe-usage.unsafe-usage
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot.is_null() || snapshot == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
        return Vec::new();
    }
    // SAFETY: the raw snapshot handle is owned exclusively and wrapped.
    let _snapshot_guard = unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(snapshot) };
    // nosemgrep: Semgrep_rust.lang.security.unsafe-usage.unsafe-usage
    let self_pid = unsafe { GetCurrentProcessId() };
    let mut children = Vec::new();
    // SAFETY: `entry.dwSize` initialized before each API call; snapshot valid.
    unsafe {
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                if entry.th32ParentProcessID == self_pid {
                    children.push(entry.th32ProcessID);
                }
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
                entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            }
        }
    }
    children
}

/// Whether `pid` is currently a member of `job` (test evidence only).
#[cfg(windows)]
#[cfg(test)]
fn process_in_job(pid: u32, job: &std::os::windows::io::OwnedHandle) -> std::io::Result<bool> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use windows_sys::Win32::Foundation::FALSE;
    use windows_sys::Win32::System::JobObjects::IsProcessInJob;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    // SAFETY: pid is a live process in tests; handle exclusively owned here.
    // nosemgrep: Semgrep_rust.lang.security.unsafe-usage.unsafe-usage
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the raw process handle is owned exclusively and wrapped.
    let process = unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(process) };
    let mut in_job = FALSE;
    // SAFETY: both handles are valid and exclusively owned.
    // nosemgrep: Semgrep_rust.lang.security.unsafe-usage.unsafe-usage
    let ok = unsafe {
        IsProcessInJob(
            process.as_raw_handle().cast(),
            job.as_raw_handle().cast(),
            &mut in_job,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(in_job != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    // —— backend identity and honest verdicts (cross-platform, pure) ——

    #[test]
    fn reports_partial_enforcement_under_stable_name() {
        let backend = WindowsJobBackend;
        assert_eq!(backend.backend_name(), "job");
        assert_eq!(backend.enforcement(), EnforcementLevel::Partial);
    }

    #[test]
    fn probe_requires_windows_target_and_job_creation() {
        assert!(WindowsJobBackend::probe_combine(true, true));
        assert!(!WindowsJobBackend::probe_combine(true, false));
        assert!(!WindowsJobBackend::probe_combine(false, true));
        assert!(!WindowsJobBackend::probe_combine(false, false));
    }

    #[test]
    fn unavailable_off_windows_or_without_probe() {
        let backend = WindowsJobBackend;
        #[cfg(not(target_os = "windows"))]
        assert!(!backend.is_available());
        // Fail-closed: a confinement attempt without availability never
        // succeeds, and a non-Windows compile names the platform.
        if !backend.is_available() {
            let cmd = tokio::process::Command::new("cmd");
            let profile = ConfinementProfile::for_shell(Path::new("C:\\Windows\\Temp"));
            let err = backend
                .spawn_confined(cmd, &profile)
                .expect_err("must fail closed");
            match err {
                SandboxError::Unavailable { platform } => {
                    #[cfg(not(target_os = "windows"))]
                    assert_eq!(platform, std::env::consts::OS);
                    #[cfg(target_os = "windows")]
                    assert_eq!(platform, "windows");
                }
                other => panic!("expected Unavailable, got: {other:?}"),
            }
        }
    }

    #[test]
    fn reason_documents_remaining_gap_and_fail_closed() {
        assert!(WINDOWS_UNAVAILABLE_REASON.contains("no filesystem write boundary"));
        assert!(WINDOWS_UNAVAILABLE_REASON.contains("no network policy"));
        assert!(WINDOWS_UNAVAILABLE_REASON.contains("fail closed"));
    }

    /// The status-line appendix is the short form of the long-form reason:
    /// every claim it makes must stay backed by
    /// [`WINDOWS_UNAVAILABLE_REASON`], so the one-liner cannot drift into
    /// under- or overstating what the backend holds.
    #[test]
    fn status_gap_summary_agrees_with_the_backend_reason() {
        assert_eq!(
            WindowsJobBackend.status_appendix(),
            Some(JOB_STATUS_GAP),
            "the job backend must declare its own status appendix"
        );
        for phrase in [
            "process-tree lifetime control",
            "no filesystem write boundary",
            "no network policy",
        ] {
            assert!(JOB_STATUS_GAP.contains(phrase), "summary omits {phrase:?}");
            assert!(
                WINDOWS_UNAVAILABLE_REASON.contains(phrase),
                "the backend no longer documents {phrase:?}: update the summary"
            );
        }
        // Full backends keep the bare status line (no appendix to append).
        assert_eq!(crate::os::UnavailableBackend.status_appendix(), None);
    }

    // —— live behavior (Windows only; run on a real host) ——

    /// Serializes the live tests: each spawns real children of this test
    /// process, and the watcher assigns committed pids — pairing is exact,
    /// but interleaved runs still share the one pending queue, so the
    /// oldest-pending pairing order stays predictable.
    #[cfg(windows)]
    static LIVE_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Availability probe: on a healthy Windows host a configured job can be
    /// created, so the backend joins the chain.
    #[cfg(windows)]
    #[test]
    fn available_on_windows_host() {
        assert!(WindowsJobBackend.is_available());
        assert_eq!(WindowsJobBackend.enforcement(), EnforcementLevel::Partial);
    }

    /// Smoke test through the trait seam exactly as the shell tool uses it:
    /// `spawn_confined` arms the command, the caller spawns it and commits
    /// the pid, and the watcher assigns + resumes the child (which then
    /// completes normally).
    #[cfg(windows)]
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn confined_shell_runs_and_completes() {
        use std::process::Stdio;
        use std::time::Duration;

        let _serial = crate::lock(&LIVE_TESTS);
        let backend = WindowsJobBackend;
        assert!(backend.is_available());
        let profile = ConfinementProfile::for_shell(Path::new("C:\\Windows\\Temp"));
        let mut cmd = tokio::process::Command::new("cmd");
        cmd.args(["/c", "echo ok"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let armed = backend
            .spawn_confined(cmd, &profile)
            .expect("confinement arms on a Windows host");
        // The watcher must pair the committed pid and resume the suspended
        // child; a generous timeout turns a watcher regression into a test
        // failure, not a hang.
        let run = async {
            let child = armed.spawn()?;
            child.wait_with_output().await
        };
        let out = tokio::time::timeout(Duration::from_secs(30), run)
            .await
            .expect("watcher must resume the armed child in time")
            .expect("spawn + wait");
        assert!(out.status.success(), "confined echo must succeed");
        assert!(String::from_utf8_lossy(&out.stdout).contains("ok"));
    }

    /// Pairing is by arm id: a child spawned beside a held guard is not
    /// assigned that guard's job (it runs and exits), and the armed command
    /// still completes once [`ArmedSpawn::spawn`] commits its own pid.
    #[cfg(windows)]
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn sibling_child_is_not_paired_with_a_held_arm() {
        use std::process::Stdio;
        use std::time::Duration;

        let _serial = crate::lock(&LIVE_TESTS);
        let backend = WindowsJobBackend;
        assert!(backend.is_available());
        let profile = ConfinementProfile::for_shell(Path::new("C:\\Windows\\Temp"));
        let mut cmd = tokio::process::Command::new("cmd");
        cmd.args(["/c", "echo armed"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let armed = backend
            .spawn_confined(cmd, &profile)
            .expect("confinement arms on a Windows host");
        let mut sibling = tokio::process::Command::new("cmd");
        sibling
            .args(["/c", "echo side"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let side = tokio::time::timeout(
            Duration::from_secs(10),
            sibling.spawn().expect("sibling spawn").wait_with_output(),
        )
        .await
        .expect("a sibling must not be suspended by someone else's arm")
        .expect("sibling wait");
        assert!(
            side.status.success(),
            "sibling must run unconfined and exit"
        );
        let out = tokio::time::timeout(Duration::from_secs(30), async {
            let child = armed.spawn()?;
            child.wait_with_output().await
        })
        .await
        .expect("the armed child must still be resumed")
        .expect("armed spawn");
        assert!(String::from_utf8_lossy(&out.stdout).contains("armed"));
    }

    /// The headline guarantee: closing the job handle kills a long-running
    /// member process promptly (KILL_ON_JOB_CLOSE), and membership is
    /// verifiable before the kill. Uses the backend's real job mechanics
    /// (the same helpers the watcher drives after `spawn_confined`).
    #[cfg(windows)]
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn job_handle_drop_kills_long_running_child() {
        use std::process::Stdio;
        use std::time::{Duration, Instant};

        let _serial = crate::lock(&LIVE_TESTS);
        let job = create_confined_job().expect("job object");
        let mut cmd = tokio::process::Command::new("ping");
        cmd.args(["-n", "30", "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            // Same arming `spawn_confined` performs (suspended start).
            .creation_flags(CREATE_SUSPENDED);
        let mut child = cmd.spawn().expect("spawn ping");
        let pid = child.id().expect("pid");
        assign_and_resume(&job, pid).expect("assign + resume");
        assert!(process_in_job(pid, &job).expect("job membership query"));

        // Dropping the job kills the tree; the exit code of a kill-on-close
        // termination is unspecified (observed 0 and 1 across Windows
        // builds), so promptness is the assertion, not the code. A
        // `ping -n 30` finishing in well under a second can only be a kill.
        let started = Instant::now();
        drop(job); // KILL_ON_JOB_CLOSE fires: the whole tree dies
        let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .expect("job close must kill the child promptly")
            .expect("wait");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "kill must be prompt, took {:?}",
            started.elapsed()
        );
        let _ = status;
    }
}
