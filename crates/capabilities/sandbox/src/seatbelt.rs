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

use super::os::{ConfinementProfile, EnforcementLevel, SandboxBackend, SandboxError};

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
    out.push_str("(allow process-exec process-fork)\n");
    out.push_str("(allow sysctl-read)\n");
    out.push_str("(allow mach-lookup)\n");
    out.push_str("(allow ipc-posix-shm*)\n");
    for root in ["/usr", "/bin", "/sbin", "/lib", "/System", "/private/etc"] {
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

/// Cached profile file for `cwd` inside a process-kept temp dir (the file
/// must outlive `spawn_confined` — the child execs after we return — so
/// profiles are never deleted, only rewritten when the cwd set changes).
fn profile_path_for(cwd: &Path, extra_writable: &[PathBuf]) -> PathBuf {
    static CACHE: OnceLock<Mutex<std::collections::HashMap<u64, PathBuf>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    let mut hasher = DefaultHasher::new();
    cwd.hash(&mut hasher);
    extra_writable.hash(&mut hasher);
    let key = hasher.finish();
    let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(path) = guard.get(&key) {
        return path.clone();
    }
    let dir = std::env::temp_dir().join("wavecode-seatbelt");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("profile-{key:x}.sb"));
    let _ = std::fs::write(&path, render_seatbelt_profile(cwd, extra_writable));
    guard.insert(key, path.clone());
    path
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
        cmd: &mut tokio::process::Command,
        profile: &ConfinementProfile,
    ) -> Result<(), SandboxError> {
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
        let profile_path = profile_path_for(cwd, &extra);
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
        *cmd = confined;
        Ok(())
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
    }

    #[test]
    fn unavailable_off_macos() {
        let backend = SeatbeltBackend;
        #[cfg(not(target_os = "macos"))]
        assert!(!backend.is_available());
        if !backend.is_available() {
            let mut cmd = tokio::process::Command::new("sh");
            let profile = ConfinementProfile::for_shell(Path::new("/work"));
            let err = backend
                .spawn_confined(&mut cmd, &profile)
                .expect_err("must fail closed");
            assert_eq!(
                err,
                SandboxError::Unavailable {
                    platform: std::env::consts::OS,
                }
            );
        }
    }
}
