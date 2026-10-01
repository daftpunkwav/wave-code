/*! @file BwrapBackend
 *  @description Bubblewrap (bwrap) confinement backend for Linux.
 *
 *  Responsibilities:
 *  - Probe for `bwrap --version` plus a user-namespace creation test.
 *  - Rewrite one spawn into a `bwrap` prefix carrying the profile mounts.
 *  - Report Full enforcement when active.
 *
 *  This module must not depend on: tools, transport, frontend crates.
 */

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::Stdio;

use super::os::{ArmedSpawn, ConfinementProfile, EnforcementLevel, SandboxBackend, SandboxError};

/// Bubblewrap backend: rewrites the spawn into
/// `bwrap <isolation + mount flags> -- <program> <args...>`, so the child
/// runs in fresh user/pid namespaces with only the profile roots visible.
///
/// User namespaces are mandatory (`--unshare-user`): without them bwrap
/// cannot set up mounts unprivileged, so the probe requires a working
/// `unshare --user` round-trip and confinement fails closed without it.
///
/// NOTE: the rewrite replaces `cmd` wholesale (stdio, env deltas and
/// `kill_on_drop` configured beforehand are lost — `std::process::Command`
/// exposes no stdio accessors). Callers must configure stdio, env scrubbing
/// and kill-on-drop AFTER `spawn_confined` returns.
#[derive(Debug, Clone, Copy, Default)]
pub struct BwrapBackend;

impl BwrapBackend {
    /// Pure probe combination (hermetic seam): both the binary check and the
    /// user-namespace check must pass.
    pub fn probe_combine(bwrap_version_ok: bool, userns_ok: bool) -> bool {
        bwrap_version_ok && userns_ok
    }

    fn version_ok() -> bool {
        std::process::Command::new("bwrap")
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    fn userns_ok() -> bool {
        // Fast hermetic round-trip: create a user namespace mapping root to
        // self and exit. No network, no mounts, no side effects.
        std::process::Command::new("unshare")
            .args(["--user", "--map-root-user", "true"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }
}

/// Build the `bwrap` argument vector (everything after the `bwrap` program)
/// for `profile`: isolation flags, profile mounts, `--chdir`, then
/// `-- <program> <args...>`. Nonexistent roots are skipped (they grant
/// nothing, mirroring the Landlock backend).
pub fn build_bwrap_argv(
    profile: &ConfinementProfile,
    cwd: Option<&Path>,
    program: &OsStr,
    args: &[OsString],
) -> Vec<OsString> {
    let mut argv: Vec<OsString> = vec![
        "--unshare-user".into(),
        "--unshare-pid".into(),
        "--unshare-uts".into(),
        "--die-with-parent".into(),
    ];
    if !profile.allow_net {
        argv.push("--unshare-net".into());
    }
    for root in &profile.writable_roots {
        if root.exists() {
            argv.push("--bind".into());
            argv.push(root.as_os_str().to_owned());
            argv.push(root.as_os_str().to_owned());
        }
    }
    for root in &profile.readonly_roots {
        if root.exists() {
            argv.push("--ro-bind".into());
            argv.push(root.as_os_str().to_owned());
            argv.push(root.as_os_str().to_owned());
        }
    }
    // Redirections such as `echo hi > /dev/null` are common in shell
    // commands; a private /dev plus /proc keeps them working.
    argv.push("--dev".into());
    argv.push("/dev".into());
    argv.push("--proc".into());
    argv.push("/proc".into());
    if let Some(dir) = cwd {
        argv.push("--chdir".into());
        argv.push(dir.as_os_str().to_owned());
    }
    argv.push("--".into());
    argv.push(program.to_owned());
    argv.extend(args.iter().cloned());
    argv
}

impl SandboxBackend for BwrapBackend {
    fn backend_name(&self) -> &'static str {
        "bwrap"
    }

    fn enforcement(&self) -> EnforcementLevel {
        EnforcementLevel::Full
    }

    fn is_available(&self) -> bool {
        // User namespaces only exist on Linux; elsewhere there is no bwrap.
        if !cfg!(target_os = "linux") {
            return false;
        }
        Self::probe_combine(Self::version_ok(), Self::userns_ok())
    }

    fn spawn_confined(
        &self,
        cmd: tokio::process::Command,
        profile: &ConfinementProfile,
    ) -> Result<ArmedSpawn, SandboxError> {
        if !cfg!(target_os = "linux") {
            return Err(SandboxError::Unavailable {
                platform: std::env::consts::OS,
            });
        }
        if !self.is_available() {
            return Err(SandboxError::Unavailable { platform: "linux" });
        }
        let (program, args, cwd, envs) = {
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
        let argv = build_bwrap_argv(profile, cwd.as_deref(), &program, &args);
        let mut confined = tokio::process::Command::new("bwrap");
        confined.args(argv);
        if let Some(dir) = cwd {
            confined.current_dir(dir);
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
    use std::ffi::OsString;

    #[test]
    fn probe_requires_both_binary_and_userns() {
        assert!(BwrapBackend::probe_combine(true, true));
        assert!(!BwrapBackend::probe_combine(true, false));
        assert!(!BwrapBackend::probe_combine(false, true));
        assert!(!BwrapBackend::probe_combine(false, false));
    }

    #[test]
    fn reports_full_enforcement_under_stable_name() {
        let backend = BwrapBackend;
        assert_eq!(backend.backend_name(), "bwrap");
        assert_eq!(backend.enforcement(), EnforcementLevel::Full);
    }

    #[test]
    fn argv_carries_mounts_chdir_and_command() {
        // Hermetic on every host: use real tempdirs for both roots instead
        // of the shell profile's Unix system paths (absent on Windows).
        let writable = tempfile::tempdir().expect("tempdir");
        let readonly = tempfile::tempdir().expect("tempdir");
        let profile = ConfinementProfile {
            allow_net: false,
            writable_roots: vec![writable.path().to_path_buf()],
            readonly_roots: vec![readonly.path().to_path_buf()],
        };
        let argv = build_bwrap_argv(
            &profile,
            Some(writable.path()),
            OsStr::new("sh"),
            &[OsString::from("-c"), OsString::from("echo hi")],
        );
        let text: Vec<String> = argv
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(text.contains(&"--unshare-user".to_owned()));
        assert!(text.contains(&"--unshare-net".to_owned()));
        assert!(text.contains(&"--bind".to_owned()));
        assert!(text.contains(&"--ro-bind".to_owned()));
        assert!(text.contains(&writable.path().to_string_lossy().into_owned()));
        assert!(text.contains(&readonly.path().to_string_lossy().into_owned()));
        // Command follows the `--` separator verbatim.
        let sep = text.iter().position(|a| a == "--").expect("separator");
        assert_eq!(text[sep + 1], "sh");
        assert!(text[sep..].contains(&"echo hi".to_owned()));
    }

    #[test]
    fn argv_keeps_network_when_allowed_and_skips_missing_roots() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut profile = ConfinementProfile::for_shell(dir.path());
        profile.allow_net = true;
        profile
            .readonly_roots
            .push(std::path::PathBuf::from("/definitely/not/here-wavecode"));
        let argv = build_bwrap_argv(&profile, None, OsStr::new("sh"), &[]);
        let text: Vec<String> = argv
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(!text.contains(&"--unshare-net".to_owned()));
        assert!(!text.iter().any(|a| a.contains("definitely/not/here")));
        assert!(!text.contains(&"--chdir".to_owned()));
    }

    #[test]
    fn unavailable_off_linux_or_without_probe() {
        let backend = BwrapBackend;
        #[cfg(not(target_os = "linux"))]
        assert!(!backend.is_available());
        // Fail-closed: a rewrite attempt without availability never succeeds.
        if !backend.is_available() {
            let cmd = tokio::process::Command::new("sh");
            let profile = ConfinementProfile::for_shell(Path::new("/work"));
            assert!(backend.spawn_confined(cmd, &profile).is_err());
        }
    }
}
