/*!
 * @file OsSandbox
 * @description OS isolation backends behind a fail-closed seam.
 *
 * Responsibilities:
 * - Name constrained execution policies (network, writes).
 * - Define the backend seam future OS mechanisms implement.
 * - Fail closed by default: no backend means no execution.
 *
 * This module must not depend on: any other workspace crate. Mechanism
 * code (landlock, seatbelt, job objects) lives in platform modules.
 */

//! OS sandbox as explicit mechanism boundary.
//!
//! Policy (what may run) already lives in neighboring crates; this crate
//! owns mechanism (how the OS enforces it). Until a platform backend
//! lands, every spawn attempt fails closed with the platform named.

/// Constrained execution policy for one spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxPolicy {
    /// Network access inside the sandbox.
    pub allow_network: bool,
    /// Filesystem writes inside the sandbox.
    pub allow_fs_writes: bool,
}

impl SandboxPolicy {
    /// Fail-closed defaults: no network, no writes.
    pub fn locked_down() -> Self {
        Self {
            allow_network: false,
            allow_fs_writes: false,
        }
    }
}

/// Current OS family for backend selection and diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsFamily {
    /// Linux (future: landlock).
    Linux,
    /// macOS (future: seatbelt).
    MacOs,
    /// Windows (future: job objects plus ACLs).
    Windows,
    /// Anything else.
    Other,
}

impl OsFamily {
    /// Detect the compilation target family.
    pub fn current() -> Self {
        if cfg!(target_os = "linux") {
            Self::Linux
        } else if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(target_os = "windows") {
            Self::Windows
        } else {
            Self::Other
        }
    }

    /// Stable name for logs and error messages.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Linux => "linux",
            Self::MacOs => "macos",
            Self::Windows => "windows",
            Self::Other => "other",
        }
    }
}

/// OS sandbox failures.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SandboxError {
    /// No backend implements this platform yet.
    #[error("no OS sandbox backend on {platform}; refusing to run unconstrained")]
    Unavailable {
        /// Platform that lacks a backend.
        platform: &'static str,
    },
    /// The backend rejected the spawn.
    #[error("backend refused spawn: {0}")]
    Refused(String),
    /// The constrained child failed to start.
    #[error("constrained spawn failed: {0}")]
    Spawn(String),
}

/// Constrained child handle.
#[derive(Debug)]
pub struct ConstrainedChild {
    /// OS process id, when the backend reports one.
    pub pid: Option<u32>,
}

/// OS isolation backend seam.
#[async_trait::async_trait]
pub trait SandboxBackend: Send + Sync {
    /// Backend name for diagnostics.
    fn name(&self) -> &'static str;

    /// Spawn a constrained child or fail closed.
    async fn spawn_constrained(
        &self,
        program: &str,
        args: &[String],
        policy: &SandboxPolicy,
    ) -> Result<ConstrainedChild, SandboxError>;
}

/// No backend: every spawn fails closed with the platform named.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopBackend;

#[async_trait::async_trait]
impl SandboxBackend for NoopBackend {
    fn name(&self) -> &'static str {
        "noop-fail-closed"
    }

    async fn spawn_constrained(
        &self,
        _program: &str,
        _args: &[String],
        _policy: &SandboxPolicy,
    ) -> Result<ConstrainedChild, SandboxError> {
        Err(SandboxError::Unavailable {
            platform: OsFamily::current().as_str(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn noop_backend_fails_closed_with_platform() {
        let err = NoopBackend
            .spawn_constrained("ls", &[], &SandboxPolicy::locked_down())
            .await
            .unwrap_err();
        assert!(matches!(err, SandboxError::Unavailable { .. }));
        assert!(err.to_string().contains(OsFamily::current().as_str()));
    }

    #[test]
    fn locked_down_denies_everything() {
        let policy = SandboxPolicy::locked_down();
        assert!(!policy.allow_network);
        assert!(!policy.allow_fs_writes);
    }
}
