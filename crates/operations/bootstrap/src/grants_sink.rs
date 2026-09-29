/*!
 * @file GrantSink
 * @description Persists human "always allow" approvals for later sessions.
 *
 * Responsibilities:
 * - Turn one derived sandbox rule into a durable grant record.
 * - Stamp the provenance the store cannot observe (tool, session, time).
 * - Degrade to in-session-only when persistence is impossible.
 *
 * This module must not depend on: the run loop, the actor, or any frontend.
 * It forwards verdict inputs; it never rules on them.
 */

use std::path::{Path, PathBuf};

use infrastructure_base::now_secs;
use state_persistence::grants::{self, Grant};
use wavecode_sandbox::Rule;

/// Where grants live and which session wrote them.
#[derive(Debug, Clone)]
pub struct GrantSink {
    home: PathBuf,
    session: String,
}

impl GrantSink {
    /// A sink writing under `home`, attributed to `session`.
    pub fn new(home: impl AsRef<Path>, session: impl Into<String>) -> Self {
        Self {
            home: home.as_ref().to_path_buf(),
            session: session.into(),
        }
    }

    /// Store the rule a human just approved permanently; `false` when nothing
    /// landed.
    ///
    /// Failure never changes this session's verdict: the sandbox already
    /// appended the rule in memory, so a refused or unwritable grant costs
    /// persistence only. A pattern carrying wildcards is refused by the store
    /// on purpose — it compared literally here, and re-parsing it as a config
    /// rule on the next load would widen what the human approved.
    pub fn record(&self, tool: &str, rule: &Rule) -> bool {
        let grant = Grant {
            rule: rule.to_string(),
            tool: tool.to_string(),
            granted_at_secs: now_secs(),
            session: self.session.clone(),
        };
        match grants::add_grant(&self.home, &grant) {
            Ok(stored) => stored,
            Err(grants::GrantError::Wildcard(entry)) => {
                tracing::warn!(
                    "always-allow grant not persisted ({entry:?} carries a wildcard): \
                     it applies to this session only"
                );
                false
            }
            Err(error) => {
                tracing::warn!("always-allow grant not persisted: {error}");
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wavecode_protocol::PermissionMode;

    fn exact_shell(command: &str) -> Rule {
        wavecode_sandbox::Sandbox::without_rules(PermissionMode::Auto)
            .allow_always("shell", &serde_json::json!({"command": command}))
            .expect("a command always derives a rule")
    }

    #[test]
    fn an_approved_rule_lands_in_the_grant_table() {
        let dir = tempfile::tempdir().unwrap();
        let sink = GrantSink::new(dir.path(), "s1");
        assert!(sink.record("shell", &exact_shell("cargo test --locked")));

        let read = grants::load_grants(dir.path());
        assert_eq!(read.grants.len(), 1);
        assert_eq!(read.grants[0].rule, "Bash(cargo test --locked)");
        assert_eq!(read.grants[0].tool, "shell");
        assert_eq!(read.grants[0].session, "s1");
    }

    /// Re-approving the same command is not a second row.
    #[test]
    fn duplicate_rules_are_not_written_twice() {
        let dir = tempfile::tempdir().unwrap();
        let sink = GrantSink::new(dir.path(), "s1");
        assert!(sink.record("shell", &exact_shell("git status")));
        assert!(!sink.record("shell", &exact_shell("git status")));
        assert_eq!(grants::load_grants(dir.path()).grants.len(), 1);
    }

    /// The widening guard, seen from the writer side: a glob-flavoured
    /// command still exempts this session, but never becomes a stored rule.
    #[test]
    fn wildcard_carrying_rules_stay_in_session() {
        let dir = tempfile::tempdir().unwrap();
        let sink = GrantSink::new(dir.path(), "s1");
        assert!(!sink.record("shell", &exact_shell("grep -rn \"cargo test *\" .")));
        assert!(grants::load_grants(dir.path()).grants.is_empty());
    }

    #[test]
    fn an_unwritable_home_degrades_without_panicking() {
        // A file where the grants directory must be created: IO fails, the
        // caller only sees `false`.
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join(".wavecode");
        std::fs::write(&blocker, "not a directory").unwrap();
        let sink = GrantSink::new(dir.path(), "s1");
        assert!(!sink.record("shell", &exact_shell("ls")));
    }
}
