/*! @file OsSandboxBackend
 *  @description OS-level confinement backends (Linux Landlock, graceful elsewhere).
 *
 *  Responsibilities:
 *  - Describe one-spawn confinement (ConfinementProfile).
 *  - Define the SandboxBackend seam over tokio::process::Command.
 *  - Enforce Linux Landlock rulesets fail-closed; refuse elsewhere.
 *
 *  This module must not depend on: tools, transport, frontend crates.
 */

use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Confinement profile for one spawn: filesystem roots plus a network flag.
///
/// Derived from the policy verdict and the tool kind — the shell tool gets
/// its working directory writable and no network by default (see
/// [`ConfinementProfile::for_shell`]). The backend enforces exactly what the
/// profile says; it never widens access on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfinementProfile {
    /// Whether the confined child keeps network access (TCP bind/connect).
    pub allow_net: bool,
    /// Roots the child may read and write (beneath rules; single files allowed).
    pub writable_roots: Vec<PathBuf>,
    /// Roots the child may only read (traverse and execute included).
    pub readonly_roots: Vec<PathBuf>,
}

impl ConfinementProfile {
    /// Shell default: the session working directory plus the system temp dir
    /// writable, the system binary/library roots read-only, no network.
    ///
    /// The temp dir stays writable because shells routinely spill there
    /// (heredocs, `mktemp`); everything else outside the profile is denied.
    pub fn for_shell(cwd: &Path) -> Self {
        Self {
            allow_net: false,
            writable_roots: vec![cwd.to_path_buf(), PathBuf::from("/tmp")],
            readonly_roots: vec![
                PathBuf::from("/usr"),
                PathBuf::from("/lib"),
                PathBuf::from("/bin"),
            ],
        }
    }
}

/// OS confinement failures. Every variant fails closed: callers must surface
/// the error and never fall back to an unconfined spawn.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SandboxError {
    /// No backend implements this platform.
    #[error("OS confinement unavailable on {platform}")]
    Unavailable { platform: &'static str },
    /// The backend exists but confinement could not be applied.
    #[error("failed to apply OS confinement: {reason}")]
    ConfineFailed { reason: String },
}

/// Enforcement depth reported by every backend and surfaced in status
/// lines (see `chain::status_line`): Full means user-namespace isolation
/// with mount control (bwrap); Partial means kernel rulesets with known
/// gaps (Landlock v1-only, no UDP), best-effort profiles (seatbelt
/// allowlists), or the refusing fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnforcementLevel {
    Full,
    Partial,
}

impl std::fmt::Display for EnforcementLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EnforcementLevel::Full => write!(f, "Full"),
            EnforcementLevel::Partial => write!(f, "Partial"),
        }
    }
}

/// OS confinement backend seam: configure `cmd` so the child is confined
/// before it execs. Implementations must be fail-closed — any failure returns
/// `Err` and the child must never run unconfined.
///
/// Backends that rewrite the command wholesale (bwrap, seatbelt) replace
/// whatever the caller configured before (stdio included — `std` exposes no
/// stdio accessors), so callers configure stdio, env scrubbing and
/// kill-on-drop on [`ArmedSpawn::command`] after `spawn_confined` returns,
/// then create the child only through [`ArmedSpawn::spawn`].
pub trait SandboxBackend: Send + Sync + std::fmt::Debug {
    /// Whether this backend can confine on the running kernel.
    fn is_available(&self) -> bool;
    /// Stable backend name for status lines and logs.
    fn backend_name(&self) -> &'static str {
        "unknown"
    }
    /// Enforcement depth, surfaced in status lines.
    fn enforcement(&self) -> EnforcementLevel {
        EnforcementLevel::Partial
    }
    /// Appendix appended to the backend's status line when the plain
    /// `name (enforcement: .., available)` rendering would overstate the
    /// isolation the backend actually holds (known gaps, structural
    /// limits). `None` keeps the bare status line. Declared by the
    /// backend itself — consumers must never string-match
    /// [`SandboxBackend::backend_name`] to decide an appendix, or a new
    /// partial backend silently loses the disclosure.
    fn status_appendix(&self) -> Option<&'static str> {
        None
    }
    /// Arm confinement for one spawn under `profile`.
    ///
    /// The returned guard owns the command. [`ArmedSpawn::spawn`] is the
    /// only way to create the child, and it performs any platform pairing
    /// (Windows job assignment) inside that call. Dropping the guard
    /// without spawning cancels the arm.
    fn spawn_confined(
        &self,
        cmd: tokio::process::Command,
        profile: &ConfinementProfile,
    ) -> Result<ArmedSpawn, SandboxError>;
}

/// One command armed by [`SandboxBackend::spawn_confined`].
///
/// Owning the command makes an uncommitted spawn unrepresentable: the
/// child is created only by [`Self::spawn`], which runs the platform
/// pairing step before returning. Dropping the guard without spawning
/// cancels that arm, so a pending Windows job cannot be claimed by a
/// later, unrelated child.
pub struct ArmedSpawn {
    cmd: tokio::process::Command,
    on_spawned: Option<Box<dyn FnOnce(u32) + Send>>,
    on_drop: Option<Box<dyn FnOnce() + Send>>,
}

impl ArmedSpawn {
    /// An unconfined command. [`Self::spawn`] creates the child and nothing else.
    pub fn new(cmd: tokio::process::Command) -> Self {
        Self {
            cmd,
            on_spawned: None,
            on_drop: None,
        }
    }

    /// Arm `cmd` with a pairing step that runs after a successful spawn
    /// and a cancel step that runs when the guard is dropped first.
    /// Windows-only: the other backends do not pair a job with the child.
    #[cfg(windows)]
    pub(crate) fn with_pairing(
        cmd: tokio::process::Command,
        commit: impl FnOnce(u32) + Send + 'static,
        cancel: impl FnOnce() + Send + 'static,
    ) -> Self {
        Self {
            cmd,
            on_spawned: Some(Box::new(commit)),
            on_drop: Some(Box::new(cancel)),
        }
    }

    /// The command, still configurable (stdio, env, kill-on-drop, process group).
    ///
    /// Rewriting backends have already replaced the program. Callers set
    /// stdio and env here, after `spawn_confined` returns.
    pub fn command(&mut self) -> &mut tokio::process::Command {
        &mut self.cmd
    }

    /// Spawn the child and, when this arm pairs a process, commit that pid
    /// before returning. A spawn error cancels the arm via [`Drop`].
    pub fn spawn(mut self) -> std::io::Result<tokio::process::Child> {
        let child = self.cmd.spawn()?;
        if let Some(pid) = child.id()
            && let Some(commit) = self.on_spawned.take()
        {
            // The pending arm now belongs to this pid. Drop must not cancel it.
            self.on_drop.take();
            commit(pid);
        }
        Ok(child)
    }
}

impl std::fmt::Debug for ArmedSpawn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArmedSpawn")
            .field("pairs_on_spawn", &self.on_spawned.is_some())
            .finish_non_exhaustive()
    }
}

impl Drop for ArmedSpawn {
    fn drop(&mut self) {
        if let Some(cancel) = self.on_drop.take() {
            cancel();
        }
    }
}

/// Fallback backend: confinement is unavailable, so every spawn fails closed
/// naming the platform. Used on all non-Linux targets.
#[derive(Debug, Clone, Copy, Default)]
pub struct UnavailableBackend;

impl SandboxBackend for UnavailableBackend {
    fn is_available(&self) -> bool {
        false
    }

    fn backend_name(&self) -> &'static str {
        "unavailable"
    }

    fn spawn_confined(
        &self,
        _cmd: tokio::process::Command,
        _profile: &ConfinementProfile,
    ) -> Result<ArmedSpawn, SandboxError> {
        Err(SandboxError::Unavailable {
            platform: std::env::consts::OS,
        })
    }
}

/// Linux Landlock backend: arms a Landlock ruleset via a pre-exec hook, so
/// the child restricts itself after fork and before exec.
///
/// Rules: writable roots get full Landlock v1 rights, read-only roots get
/// execute/read-file/read-dir, TCP bind/connect is handled (hence denied —
/// no port rules are ever added) unless `profile.allow_net`. The ABI floor is
/// v1 with a hard requirement: kernels without Landlock, or without the
/// network ABI when network must be blocked, fail closed instead of running
/// with weaker confinement.
///
/// Known scope: only v1 rights are handled, so newer rights (cross-directory
/// rename/link, truncate, ioctl, Unix-socket resolve) stay allowed, and
/// Landlock never restricts UDP. The headline guarantees hold regardless: no
/// reads outside the allowed roots, no writes outside the writable roots, no
/// TCP when blocked.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, Default)]
pub struct LinuxLandlockBackend;

#[cfg(target_os = "linux")]
impl SandboxBackend for LinuxLandlockBackend {
    fn is_available(&self) -> bool {
        probe_landlock_v1()
    }

    fn backend_name(&self) -> &'static str {
        "landlock"
    }

    fn enforcement(&self) -> EnforcementLevel {
        // v1 rights only (newer rights stay allowed) and no UDP coverage.
        EnforcementLevel::Partial
    }

    fn spawn_confined(
        &self,
        mut cmd: tokio::process::Command,
        profile: &ConfinementProfile,
    ) -> Result<ArmedSpawn, SandboxError> {
        if !self.is_available() {
            return Err(SandboxError::Unavailable { platform: "linux" });
        }
        let profile = profile.clone();
        // SAFETY: pre_exec runs in the child after fork and before exec; the
        // closure only owns profile paths and applies the ruleset, returning
        // Err (which aborts the spawn) on any failure — fail-closed.
        // nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.as_std_mut().pre_exec(move || apply_landlock(&profile));
        }
        Ok(ArmedSpawn::new(cmd))
    }
}

/// Probe: can this kernel create a v1-hard-requirement ruleset? The created
/// ruleset is dropped immediately (its fd closes); restriction itself happens
/// per-spawn in the child.
#[cfg(target_os = "linux")]
fn probe_landlock_v1() -> bool {
    use landlock::{ABI, Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr};
    Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(ABI::V1))
        .and_then(|ruleset| ruleset.create())
        .is_ok()
}

/// Build and enforce the Landlock ruleset for `profile` in the pre-exec child.
/// Any error aborts the spawn (the child never execs unconfined).
#[cfg(target_os = "linux")]
fn apply_landlock(profile: &ConfinementProfile) -> std::io::Result<()> {
    use landlock::{
        ABI, Access, AccessFs, AccessNet, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset,
        RulesetAttr, RulesetCreatedAttr,
    };
    use std::io::Error;

    // A function, not a closure: `impl Trait` in closure parameters is
    // newer than the 1.90 MSRV. `Error::other` is the form clippy accepts.
    fn fail(what: &str, detail: impl std::fmt::Display) -> Error {
        Error::other(format!("landlock confinement failed at {what}: {detail}"))
    }
    let abi = ABI::V1;
    let ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(abi))
        .map_err(|e| fail("handle fs access", e))?;
    let ruleset = if profile.allow_net {
        ruleset
    } else {
        // Handling TCP bind/connect with no port rules denies all of it; the
        // hard requirement fails on pre-V4 kernels instead of silently
        // leaving the network open.
        ruleset
            .handle_access(AccessNet::from_all(ABI::V4))
            .map_err(|e| fail("handle net access", e))?
    };
    let mut created = ruleset.create().map_err(|e| fail("create ruleset", e))?;
    let rw = AccessFs::from_all(abi);
    let ro = AccessFs::from_read(abi);
    // Nonexistent roots grant nothing, so skipping them neither widens nor
    // narrows the sandbox (a missing required cwd fails later at spawn).
    for root in &profile.writable_roots {
        if let Ok(fd) = PathFd::new(root) {
            created = created
                .add_rule(PathBeneath::new(fd, rw))
                .map_err(|e| fail("add writable root", e))?;
        }
    }
    for root in &profile.readonly_roots {
        if let Ok(fd) = PathFd::new(root) {
            created = created
                .add_rule(PathBeneath::new(fd, ro))
                .map_err(|e| fail("add readonly root", e))?;
        }
    }
    // Redirections such as `echo hi > /dev/null` are common in shell
    // commands; allow just that file (not all of /dev) when present.
    if let Ok(fd) = PathFd::new("/dev/null") {
        created = created
            .add_rule(PathBeneath::new(fd, AccessFs::from_file(abi)))
            .map_err(|e| fail("add /dev/null", e))?;
    }
    created
        .restrict_self()
        .map_err(|e| fail("restrict self", e))?;
    Ok(())
}

/// Pick the platform backend in probe order (see `chain::PROBE_ORDER`):
/// Linux tries bwrap, then Landlock; macOS tries seatbelt; Windows tries the
/// partial Job-Object backend (see `windows.rs` — process-tree lifetime
/// control only, no filesystem / network boundary). The first available
/// backend wins; when nothing is available the refusing fallback is returned
/// and any requested confinement fails closed (`SANDBOX_UNAVAILABLE`).
pub fn detect_backend() -> Arc<dyn SandboxBackend> {
    #[cfg(target_os = "linux")]
    {
        let bwrap: Arc<dyn SandboxBackend> = Arc::new(crate::bwrap::BwrapBackend);
        if bwrap.is_available() {
            return bwrap;
        }
        let landlock: Arc<dyn SandboxBackend> = Arc::new(LinuxLandlockBackend);
        if landlock.is_available() {
            return landlock;
        }
        Arc::new(UnavailableBackend)
    }
    #[cfg(target_os = "macos")]
    {
        let seatbelt: Arc<dyn SandboxBackend> = Arc::new(crate::seatbelt::SeatbeltBackend);
        if seatbelt.is_available() {
            return seatbelt;
        }
        Arc::new(UnavailableBackend)
    }
    #[cfg(target_os = "windows")]
    {
        let job: Arc<dyn SandboxBackend> = Arc::new(crate::windows::WindowsJobBackend);
        if job.is_available() {
            return job;
        }
        Arc::new(UnavailableBackend)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        Arc::new(UnavailableBackend)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_profile_is_cwd_write_only_without_net() {
        let cwd = Path::new("/session/work");
        let profile = ConfinementProfile::for_shell(cwd);
        assert!(!profile.allow_net, "shell gets no network by default");
        assert!(profile.writable_roots.contains(&cwd.to_path_buf()));
        assert!(profile.writable_roots.contains(&PathBuf::from("/tmp")));
        for root in ["/usr", "/lib", "/bin"] {
            assert!(
                profile.readonly_roots.contains(&PathBuf::from(root)),
                "{root} must be read-only"
            );
        }
        // Writable and read-only sets must not overlap.
        for root in &profile.writable_roots {
            assert!(
                !profile.readonly_roots.contains(root),
                "{root:?} must not be both writable and read-only"
            );
        }
    }

    #[test]
    fn unavailable_backend_refuses_naming_the_platform() {
        let backend = UnavailableBackend;
        assert!(!backend.is_available());
        let cmd = tokio::process::Command::new("sh");
        let profile = ConfinementProfile::for_shell(Path::new("/work"));
        let err = backend
            .spawn_confined(cmd, &profile)
            .expect_err("fallback must never arm confinement");
        assert_eq!(
            err,
            SandboxError::Unavailable {
                platform: std::env::consts::OS,
            }
        );
        assert!(
            err.to_string().contains("OS confinement unavailable on"),
            "error names the failure: {err}"
        );
    }

    #[test]
    fn detect_backend_matches_platform() {
        let backend = detect_backend();
        // Availability is kernel-dependent on Linux/macOS, host-dependent on
        // Windows; elsewhere it is always the refusing fallback.
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        assert!(!backend.is_available());
        #[cfg(target_os = "linux")]
        {
            // Chain order: bwrap wins when available, else Landlock, else the
            // refusing fallback (fail-closed only when neither is available).
            if backend.is_available() {
                assert!(
                    ["bwrap", "landlock"].contains(&backend.backend_name()),
                    "unexpected backend: {}",
                    backend.backend_name()
                );
            } else {
                assert!(!crate::bwrap::BwrapBackend.is_available());
                assert!(!LinuxLandlockBackend.is_available());
            }
        }
        #[cfg(target_os = "macos")]
        {
            if backend.is_available() {
                assert_eq!(backend.backend_name(), "seatbelt");
            } else {
                assert!(!crate::seatbelt::SeatbeltBackend.is_available());
            }
        }
        #[cfg(target_os = "windows")]
        {
            // Chain order: the Job-Object backend wins when available, else
            // the refusing fallback (fail-closed only when it cannot arm).
            if backend.is_available() {
                assert_eq!(backend.backend_name(), "job");
            } else {
                assert!(!crate::windows::WindowsJobBackend.is_available());
            }
        }
    }

    /// Live confinement: a confined shell writes inside the cwd but cannot
    /// read a file that exists outside the allowed roots. Skips (passes)
    /// where the kernel has no Landlock — availability is environmental.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn confined_shell_writes_inside_cwd_only() {
        use std::process::Stdio;

        let backend = LinuxLandlockBackend;
        if !backend.is_available() {
            return;
        }
        let inside = tempfile::tempdir().expect("tempdir");
        // `for_shell` also grants `/tmp`. A second system temp dir would
        // sit inside that root, so the secret has to live under the
        // process cwd, which the profile does not allow.
        let outside =
            tempfile::tempdir_in(std::env::current_dir().expect("cwd")).expect("outside dir");
        std::fs::write(outside.path().join("secret.txt"), "secret").expect("seed file");
        let profile = ConfinementProfile::for_shell(inside.path());

        // Inside write succeeds (proves the shell runs under confinement).
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg("echo hello > marker.txt")
            .current_dir(inside.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let out = backend
            .spawn_confined(cmd, &profile)
            .expect("confinement arms on a Landlock kernel")
            .spawn()
            .expect("spawn")
            .wait_with_output()
            .await
            .expect("wait");
        assert!(out.status.success(), "cwd write must succeed");
        let marker = std::fs::read_to_string(inside.path().join("marker.txt")).expect("marker");
        assert!(marker.contains("hello"));

        // Outside read fails even though the file exists.
        let mut cmd = tokio::process::Command::new("sh");
        let script = format!("cat {}", outside.path().join("secret.txt").display());
        cmd.arg("-c")
            .arg(script)
            .current_dir(inside.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let out = backend
            .spawn_confined(cmd, &profile)
            .expect("confinement arms on a Landlock kernel")
            .spawn()
            .expect("spawn")
            .wait_with_output()
            .await
            .expect("wait");
        assert!(
            !out.status.success(),
            "read outside the allowed roots must fail"
        );
        assert!(
            !String::from_utf8_lossy(&out.stdout).contains("secret"),
            "confined child must not exfiltrate the outside file"
        );

        // Outside write fails too.
        let mut cmd = tokio::process::Command::new("sh");
        let script = format!("echo hi > {}", outside.path().join("nope.txt").display());
        cmd.arg("-c")
            .arg(script)
            .current_dir(inside.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let out = backend
            .spawn_confined(cmd, &profile)
            .expect("confinement arms on a Landlock kernel")
            .spawn()
            .expect("spawn")
            .wait_with_output()
            .await
            .expect("wait");
        assert!(
            !out.status.success(),
            "write outside the writable roots must fail"
        );
        assert!(
            !outside.path().join("nope.txt").exists(),
            "no file may appear outside the writable roots"
        );
    }

    /// Live network confinement: a confined child cannot fetch from loopback.
    /// Skips where Landlock or curl is missing; a fail-closed spawn error
    /// (no network ABI) counts as confined.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn confined_child_cannot_fetch_loopback() {
        use std::io::Write;
        use std::process::Stdio;
        use std::time::Duration;

        let backend = LinuxLandlockBackend;
        if !backend.is_available() {
            return;
        }
        let has_curl = tokio::process::Command::new("sh")
            .arg("-c")
            .arg("command -v curl")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .is_ok_and(|s| s.success());
        if !has_curl {
            return;
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        let port = listener.local_addr().expect("port").port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let _ = stream.write_all(b"MARKER");
                std::thread::sleep(Duration::from_millis(500));
            }
        });
        let dir = tempfile::tempdir().expect("tempdir");
        let profile = ConfinementProfile::for_shell(dir.path());
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg(format!("curl -s -m 5 http://127.0.0.1:{port}/"))
            .current_dir(dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        match backend.spawn_confined(cmd, &profile) {
            // No network ABI: fail-closed without spawning counts as confined.
            Err(_) => {}
            Ok(armed) => {
                let out = armed
                    .spawn()
                    .expect("spawn")
                    .wait_with_output()
                    .await
                    .expect("wait");
                assert!(
                    !String::from_utf8_lossy(&out.stdout).contains("MARKER"),
                    "confined child must not fetch loopback"
                );
            }
        }
    }
}
