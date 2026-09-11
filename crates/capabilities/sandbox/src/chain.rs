/*! @file SandboxChain
 *  @description Cross-platform backend probe chain and status lines.
 *
 *  Responsibilities:
 *  - Define the probe order (bwrap -> Landlock -> seatbelt -> Windows).
 *  - Pick the first available backend (fail-closed when none is available).
 *  - Render one-line backend status surfacing the enforcement level.
 *
 *  This module must not depend on: tools, transport, frontend crates.
 */

use std::sync::Arc;

use super::os::{SandboxBackend, UnavailableBackend};

/// Probe order, highest-priority first: Linux bwrap, Linux Landlock, macOS
/// seatbelt. Windows has no enforcing backend (see `windows.rs`), so the
/// chain ends at the refusing fallback there.
pub const PROBE_ORDER: [&str; 3] = ["bwrap", "landlock", "seatbelt"];

/// First-available-wins over an explicit candidate list (the hermetic seam
/// for the probe order: tests pass fakes, production passes the live
/// backends in [`PROBE_ORDER`]).
pub fn first_available<'a>(
    backends: impl IntoIterator<Item = &'a Arc<dyn SandboxBackend>>,
) -> Option<&'a Arc<dyn SandboxBackend>> {
    backends.into_iter().find(|b| b.is_available())
}

/// One-line status for a backend: name, enforcement level and availability.
/// Unavailable backends carry the `SANDBOX_UNAVAILABLE` token so fail-closed
/// refusals are greppable in logs.
pub fn status_line(backend: &Arc<dyn SandboxBackend>) -> String {
    if backend.is_available() {
        format!(
            "{} (enforcement: {}, available)",
            backend.backend_name(),
            backend.enforcement()
        )
    } else {
        format!(
            "{} (enforcement: {}, SANDBOX_UNAVAILABLE)",
            backend.backend_name(),
            backend.enforcement()
        )
    }
}

/// The refusing fallback as a trait object (chain terminus on platforms with
/// no enforcing backend).
pub fn unavailable() -> Arc<dyn SandboxBackend> {
    Arc::new(UnavailableBackend)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::os::{ConfinementProfile, EnforcementLevel, SandboxError};

    #[derive(Debug)]
    struct FakeBackend {
        name: &'static str,
        available: bool,
        level: EnforcementLevel,
    }

    impl SandboxBackend for FakeBackend {
        fn is_available(&self) -> bool {
            self.available
        }

        fn backend_name(&self) -> &'static str {
            self.name
        }

        fn enforcement(&self) -> EnforcementLevel {
            self.level
        }

        fn spawn_confined(
            &self,
            _cmd: &mut tokio::process::Command,
            _profile: &ConfinementProfile,
        ) -> Result<(), SandboxError> {
            if self.available {
                Ok(())
            } else {
                Err(SandboxError::Unavailable { platform: "fake" })
            }
        }
    }

    fn fake(
        name: &'static str,
        available: bool,
        level: EnforcementLevel,
    ) -> Arc<dyn SandboxBackend> {
        Arc::new(FakeBackend {
            name,
            available,
            level,
        })
    }

    #[test]
    fn first_available_wins_in_probe_order() {
        let bwrap = fake("bwrap", true, EnforcementLevel::Full);
        let landlock = fake("landlock", true, EnforcementLevel::Partial);
        let seatbelt = fake("seatbelt", true, EnforcementLevel::Partial);
        let picked = first_available([&bwrap, &landlock, &seatbelt]).expect("one is available");
        assert_eq!(picked.backend_name(), "bwrap");
    }

    #[test]
    fn chain_falls_through_to_landlock_then_seatbelt() {
        let bwrap = fake("bwrap", false, EnforcementLevel::Full);
        let landlock = fake("landlock", true, EnforcementLevel::Partial);
        let seatbelt = fake("seatbelt", true, EnforcementLevel::Partial);
        let picked = first_available([&bwrap, &landlock, &seatbelt]).expect("landlock up");
        assert_eq!(picked.backend_name(), "landlock");

        let landlock = fake("landlock", false, EnforcementLevel::Partial);
        let picked = first_available([&bwrap, &landlock, &seatbelt]).expect("seatbelt up");
        assert_eq!(picked.backend_name(), "seatbelt");
    }

    #[test]
    fn nothing_available_is_fail_closed() {
        let bwrap = fake("bwrap", false, EnforcementLevel::Full);
        let landlock = fake("landlock", false, EnforcementLevel::Partial);
        assert!(first_available([&bwrap, &landlock]).is_none());
        // The chain terminus refuses every spawn.
        let end = unavailable();
        assert!(!end.is_available());
        let mut cmd = tokio::process::Command::new("sh");
        let profile =
            ConfinementProfile::for_shell(std::path::Path::new("/work"));
        assert!(end.spawn_confined(&mut cmd, &profile).is_err());
    }

    #[test]
    fn status_lines_surface_enforcement_and_unavailability() {
        let full = fake("bwrap", true, EnforcementLevel::Full);
        let line = status_line(&full);
        assert!(line.contains("bwrap"));
        assert!(line.contains("Full"));
        assert!(line.contains("available"));
        assert!(!line.contains("SANDBOX_UNAVAILABLE"));

        let none = unavailable();
        let line = status_line(&none);
        assert!(line.contains("SANDBOX_UNAVAILABLE"));
        assert!(line.contains("Partial"));
    }
}
