/*! @file SeatbeltBackend
 *  @description macOS seatbelt (`sandbox-exec`) confinement backend.
 *
 *  Responsibilities:
 *  - Probe for `/usr/bin/sandbox-exec` on macOS.
 *  - Render a Seatbelt profile denying writes outside the session cwd.
 *  - Rewrite one spawn into a `sandbox-exec -f <profile>` prefix.
 *  - Report Partial enforcement (profile gaps are documented).
 *
 *  This module must not depend on: tools, transport, frontend crates.
 */

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use super::os::{ArmedSpawn, ConfinementProfile, EnforcementLevel, SandboxBackend, SandboxError};

/// Seatbelt backend: prefixes the spawn with
/// `/usr/bin/sandbox-exec -f <generated profile>`, so the child may read
/// system roots but may write only under the session working directory (plus
/// the system temp dir shells spill into).
///
/// Enforcement is Partial by construction: Seatbelt profiles are an
/// allowlist with coarse operations, and network access is not restricted
/// per-process here (no `deny network*` rule - shells spawned through this
/// backend keep network access; network denial stays a Landlock/bwrap
/// guarantee).
///
/// NOTE: like the bwrap backend this rewrites `cmd` wholesale — callers
/// must configure stdio, env scrubbing and kill-on-drop AFTER
/// `spawn_confined` returns.
#[derive(Debug, Clone, Copy, Default)]
pub struct SeatbeltBackend;

impl SeatbeltBackend {
    /// Hermetic availability seam: the real check is macOS plus the
    /// `sandbox-exec` binary.
    pub fn probe_combine(is_macos: bool, binary_present: bool) -> bool {
        is_macos && binary_present
    }
}

/// Render the Seatbelt profile for `cwd`: deny-by-default, read system
/// roots, write only `cwd` / temp / /dev/null. Pure and hermetic.
pub fn render_seatbelt_profile(cwd: &Path, extra_writable: &[PathBuf]) -> String {
    let mut out = String::from("(version 1)\n(deny default)\n");
    // dyld and libsystem run before main. Without these, sandbox-exec
    // SIGKILLs the child and the parent only sees an empty signal exit.
    out.push_str("(allow process-exec process-fork)\n");
    out.push_str("(allow process-info*)\n");
    out.push_str("(allow signal (target self))\n");
    out.push_str("(allow sysctl-read)\n");
    out.push_str("(allow mach-lookup)\n");
    out.push_str("(allow mach-priv-host-port)\n");
    out.push_str("(allow mach-register)\n");
    out.push_str("(allow ipc-posix-shm*)\n");
    out.push_str("(allow file-map-executable)\n");
    out.push_str("(allow file-read-metadata)\n");
    // `/etc` is a symlink to `/private/etc`; seatbelt checks the path
    // the caller used, so both must be readable. `/opt` covers Homebrew.
    for root in [
        "/usr",
        "/bin",
        "/sbin",
        "/lib",
        "/System",
        "/Library",
        "/etc",
        "/private/etc",
        "/opt",
    ] {
        out.push_str(&format!("(allow file-read* (subpath \"{root}\"))\n"));
    }
    out.push_str(&format!(
        "(allow file-read* file-write-data file-write-create file-write-unlink (subpath \"{}\"))\n",
        cwd.display()
    ));
    for extra in extra_writable {
        out.push_str(&format!(
            "(allow file-read* file-write-data file-write-create file-write-unlink (subpath \"{}\"))\n",
            extra.display()
        ));
    }
    out.push_str("(allow file-read* file-write-data (literal \"/dev/null\"))\n");
    out
}

/// Directory holding this process's generated profiles: pid-scoped under
/// the system temp dir, created by this process with owner-only
/// permissions on unix. A shared, predictable directory would let a local
/// attacker pre-create it (owning it), then swap profile files between
/// the write here and `sandbox-exec`'s read — weakening the confinement
/// the child runs under. Returns `None` when no acceptable directory can
/// be established; callers must fail closed on that.
fn profile_dir() -> Option<PathBuf> {
    // nosemgrep: Semgrep_rust.lang.security.temp-dir.temp-dir
    let temp = std::env::temp_dir();
    let primary = temp.join(format!("wavecode-seatbelt-{}", std::process::id()));
    if private_profile_dir(&primary) {
        return Some(primary);
    }
    // The primary name is polluted (wrong permissions or owner): fall
    // back to per-process random suffixes so a hostile leftover cannot
    // force profile writes into a directory it controls.
    for _ in 0..4 {
        let candidate = temp.join(format!(
            "wavecode-seatbelt-{}-{:x}",
            std::process::id(),
            fallback_key()
        ));
        if private_profile_dir(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Establish `dir` as a profile directory this process owns: created here
/// and locked to the owner on unix (`0o700`), or accepted when an existing
/// directory is already owner-only AND owned by this effective user. The
/// owner check is what makes the mode check meaningful: running as root,
/// a write into a foreign-owned `0o700` directory would succeed and the
/// directory's owner could then swap the profile between the write and
/// `sandbox-exec`'s read; a foreign-owned directory otherwise makes every
/// profile write fail, which callers turn into a refused spawn rather
/// than a confinement bypass.
fn private_profile_dir(dir: &Path) -> bool {
    match std::fs::create_dir(dir) {
        Ok(()) => {
            #[cfg(unix)]
            {
                set_owner_only(dir) && is_owner_only(dir)
            }
            #[cfg(not(unix))]
            {
                true
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            #[cfg(unix)]
            {
                is_owner_only(dir)
            }
            #[cfg(not(unix))]
            {
                dir.is_dir()
            }
        }
        Err(_) => false,
    }
}

#[cfg(unix)]
fn set_owner_only(dir: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let permissions = std::fs::Permissions::from_mode(0o700);
    std::fs::set_permissions(dir, permissions).is_ok()
}

#[cfg(unix)]
fn is_owner_only(dir: &Path) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    match std::fs::symlink_metadata(dir) {
        // Reject symlinks outright: a link planted over the expected name
        // must never resolve into attacker-controlled territory.
        Ok(meta) => {
            meta.is_dir()
                && meta.permissions().mode() & 0o777 == 0o700
                && meta.uid() == current_uid()
        }
        Err(_) => false,
    }
}

#[cfg(unix)]
fn current_uid() -> u32 {
    // SAFETY: `geteuid` is thread-free and has no preconditions.
    // nosemgrep: Semgrep_rust.lang.security.unsafe-usage.unsafe-usage
    unsafe { libc::geteuid() }
}

/// Random per-process suffix for fallback directory names, seeded from
/// `RandomState`'s per-process random keys plus the clock.
fn fallback_key() -> u64 {
    use std::hash::{BuildHasher, Hash, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    std::process::id().hash(&mut hasher);
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default()
        .hash(&mut hasher);
    hasher.finish()
}

/// Cached profile file for `cwd` inside the process-private profile
/// directory (the file must outlive `spawn_confined` — the child execs
/// after we return — so profiles are never deleted, only rewritten when
/// the cwd set changes). Fails closed: an unwritable profile directory or
/// file refuses the spawn instead of letting `sandbox-exec` read a stale
/// or foreign profile.
fn profile_path_for(cwd: &Path, extra_writable: &[PathBuf]) -> std::io::Result<PathBuf> {
    static CACHE: OnceLock<Mutex<std::collections::HashMap<u64, PathBuf>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    let mut hasher = DefaultHasher::new();
    cwd.hash(&mut hasher);
    extra_writable.hash(&mut hasher);
    let key = hasher.finish();
    // The profile is rewritten on every call, cache hit or not: the child
    // execs after this returns, so the file it reads must hold exactly
    // this render even if another thread served the same key between the
    // cache lookup and the write. (The 64-bit key is only an in-process
    // path label — the directory is pid-scoped, so cross-session
    // interference is out of scope.)
    let rendered = render_seatbelt_profile(cwd, extra_writable);
    let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    let dir = profile_dir().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "no private seatbelt profile directory could be established",
        )
    })?;
    let path = guard
        .get(&key)
        .cloned()
        .unwrap_or_else(|| dir.join(format!("profile-{key:x}.sb")));
    std::fs::write(&path, rendered)?;
    guard.insert(key, path.clone());
    Ok(path)
}

impl SandboxBackend for SeatbeltBackend {
    fn backend_name(&self) -> &'static str {
        "seatbelt"
    }

    fn enforcement(&self) -> EnforcementLevel {
        EnforcementLevel::Partial
    }

    fn is_available(&self) -> bool {
        Self::probe_combine(
            cfg!(target_os = "macos"),
            Path::new("/usr/bin/sandbox-exec").exists(),
        )
    }

    fn spawn_confined(
        &self,
        cmd: tokio::process::Command,
        profile: &ConfinementProfile,
    ) -> Result<ArmedSpawn, SandboxError> {
        if !self.is_available() {
            return Err(SandboxError::Unavailable {
                platform: std::env::consts::OS,
            });
        }
        let cwd = profile
            .writable_roots
            .first()
            .map(PathBuf::as_path)
            .unwrap_or_else(|| Path::new("/tmp"));
        let extra: Vec<PathBuf> = profile.writable_roots.iter().skip(1).cloned().collect();
        let profile_path =
            profile_path_for(cwd, &extra).map_err(|error| SandboxError::ConfineFailed {
                reason: format!("seatbelt profile unavailable: {error}"),
            })?;
        let (program, args, dir, envs) = {
            let view = cmd.as_std();
            (
                view.get_program().to_owned(),
                view.get_args().map(|a| a.to_owned()).collect::<Vec<_>>(),
                view.get_current_dir().map(|p| p.to_owned()),
                view.get_envs()
                    .map(|(k, v)| (k.to_owned(), v.map(|v| v.to_owned())))
                    .collect::<Vec<_>>(),
            )
        };
        let mut confined = tokio::process::Command::new("/usr/bin/sandbox-exec");
        confined
            .arg("-f")
            .arg(&profile_path)
            .arg(program)
            .args(args);
        if let Some(dir) = dir {
            confined.current_dir(dir);
        } else {
            confined.current_dir(cwd);
        }
        for (key, value) in envs {
            match value {
                Some(v) => {
                    confined.env(key, v);
                }
                None => {
                    confined.env_remove(key);
                }
            }
        }
        Ok(ArmedSpawn::new(confined))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_requires_macos_and_binary() {
        assert!(SeatbeltBackend::probe_combine(true, true));
        assert!(!SeatbeltBackend::probe_combine(true, false));
        assert!(!SeatbeltBackend::probe_combine(false, true));
        assert!(!SeatbeltBackend::probe_combine(false, false));
    }

    #[test]
    fn reports_partial_enforcement_under_stable_name() {
        let backend = SeatbeltBackend;
        assert_eq!(backend.backend_name(), "seatbelt");
        assert_eq!(backend.enforcement(), EnforcementLevel::Partial);
    }

    #[test]
    fn profile_denies_writes_outside_cwd() {
        let cwd = Path::new("/session/work");
        let profile = render_seatbelt_profile(cwd, &[]);
        assert!(profile.contains("(deny default)"));
        assert!(profile.contains("(subpath \"/session/work\")"));
        assert!(
            !profile.contains("(allow file-write*)\n"),
            "no blanket write allow may appear: {profile}"
        );
        // Reads stay broad (system roots), writes stay scoped.
        assert!(profile.contains("(allow file-read* (subpath \"/usr\"))"));
        assert!(profile.contains("(allow file-map-executable)"));
        assert!(profile.contains("(allow file-read-metadata)"));
    }

    #[test]
    fn unavailable_off_macos() {
        let backend = SeatbeltBackend;
        #[cfg(not(target_os = "macos"))]
        assert!(!backend.is_available());
        if !backend.is_available() {
            let cmd = tokio::process::Command::new("sh");
            let profile = ConfinementProfile::for_shell(Path::new("/work"));
            let err = backend
                .spawn_confined(cmd, &profile)
                .expect_err("must fail closed");
            assert_eq!(
                err,
                SandboxError::Unavailable {
                    platform: std::env::consts::OS,
                }
            );
        }
    }

    #[test]
    fn profile_dir_is_scoped_to_this_process() {
        // nosemgrep: Semgrep_rust.lang.security.temp-dir.temp-dir
        let primary =
            std::env::temp_dir().join(format!("wavecode-seatbelt-{}", std::process::id()));
        // A leftover from a previous run of this pid would be accepted as
        //-is; drop it so the test observes the create path it asserts on.
        let _ = std::fs::remove_dir(&primary);
        let dir = profile_dir().expect("a profile directory must be established");
        assert_eq!(dir, primary, "primary profile dir must be pid-scoped");
    }

    /// A pre-existing directory that is not locked to its owner must be
    /// rejected: it may be a hostile leftover trying to capture the
    /// profile writes (unix checks real mode bits; other platforms only
    /// have the directory check).
    #[test]
    fn profile_dir_rejects_unlocked_precreated_directory() {
        // nosemgrep: Semgrep_rust.lang.security.temp-dir.temp-dir
        let base = std::env::temp_dir().join(format!(
            "wavecode-seatbelt-reject-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir(&base).expect("test dir creation");
        #[cfg(unix)]
        {
            // The acceptable pre-existing shape is the owner-locked one;
            // a default-permission directory is (correctly) rejected.
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700))
                .expect("lock base down");
        }
        assert!(private_profile_dir(&base));

        let hostile = base.join("hostile");
        std::fs::create_dir(&hostile).expect("hostile dir creation");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hostile, std::fs::Permissions::from_mode(0o777))
                .expect("open the hostile dir");
            assert!(
                !private_profile_dir(&hostile),
                "a world-accessible directory must never be accepted"
            );

            // One locked down by us (0o700) is the expected reusable shape.
            let ours = base.join("ours");
            std::fs::create_dir(&ours).expect("ours dir creation");
            std::fs::set_permissions(&ours, std::fs::Permissions::from_mode(0o700))
                .expect("lock ours down");
            assert!(private_profile_dir(&ours));
        }
        #[cfg(not(unix))]
        assert!(private_profile_dir(&hostile));

        std::fs::remove_dir_all(&base).ok();
    }

    /// A directory that cannot be established as private — here because a
    /// `0o500` parent forbids creating it — is rejected rather than
    /// accepted by name; `profile_path_for` turns such rejections into an
    /// error, which `spawn_confined` maps to `ConfineFailed` (fail-closed
    /// for the spawn).
    #[cfg(unix)]
    #[test]
    fn private_profile_dir_rejects_when_creation_is_forbidden() {
        use std::os::unix::fs::PermissionsExt;
        // nosemgrep: Semgrep_rust.lang.security.temp-dir.temp-dir
        let base = std::env::temp_dir().join(format!(
            "wavecode-seatbelt-failclosed-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir(&base).expect("base dir");
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o500))
            .expect("make base read-only");
        let locked = base.join("locked");
        assert!(
            !private_profile_dir(&locked),
            "0o500 parent forbids creation and a missing dir is never accepted"
        );
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700))
            .expect("restore base");
        std::fs::remove_dir_all(&base).ok();
    }
}
