//! `wavecode grants`: inspect and revoke the persisted always-allow
//! table under `~/.wavecode/grants.jsonl`. Offline only.

use std::path::Path;

use crate::GrantsCommand;

/// `grants`: inspect and revoke the persisted always-allow table.
///
/// Every action here only tightens authority (a revoked grant goes back to
/// asking), so no confirmation gate is warranted. The exit code separates
/// "nothing to do" from "could not tell".
pub(crate) fn run_grants(home: Option<&Path>, command: GrantsCommand) -> bool {
    use state_persistence::grants;
    let Some(home) = home else {
        eprintln!("grants: no home directory available");
        return false;
    };
    let read = grants::load_grants(home);
    if read.malformed > 0 {
        eprintln!(
            "[warn] {} malformed grant line(s) skipped in {}",
            read.malformed,
            grants::grants_path(home).display()
        );
    }
    match command {
        GrantsCommand::List => {
            println!(
                "grants {} ({} stored)",
                grants::grants_path(home).display(),
                read.grants.len()
            );
            for (index, grant) in read.grants.iter().enumerate() {
                println!(
                    "  {index:<3} {:<48} {:<8} {}",
                    grant.rule, grant.tool, grant.session
                );
            }
            true
        }
        GrantsCommand::Remove { index } => match grants::remove_grant(home, index) {
            Ok(Some(grant)) => {
                println!("revoked {}", grant.rule);
                true
            }
            Ok(None) => {
                eprintln!(
                    "grants: no grant at index {index} ({} stored)",
                    read.grants.len()
                );
                false
            }
            Err(error) => {
                eprintln!("grants: {error}");
                false
            }
        },
        GrantsCommand::Clear => match grants::clear_grants(home) {
            Ok(count) => {
                println!("revoked {count} grant(s)");
                true
            }
            Err(error) => {
                eprintln!("grants: {error}");
                false
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::run_grants;
    use crate::GrantsCommand;

    /// Grants CLI contract: list always answers, revoke reports what it
    /// could not find, and a real removal empties the table.
    #[test]
    fn grants_list_remove_and_clear() {
        let dir = tempfile::tempdir().unwrap();
        assert!(run_grants(Some(dir.path()), GrantsCommand::List));
        let grant = state_persistence::grants::Grant {
            rule: "Bash(cargo fmt)".to_string(),
            tool: "shell".to_string(),
            granted_at_secs: 1,
            session: "s1".to_string(),
        };
        state_persistence::grants::add_grant(dir.path(), &grant).unwrap();

        assert!(!run_grants(
            Some(dir.path()),
            GrantsCommand::Remove { index: 7 }
        ));
        assert!(run_grants(
            Some(dir.path()),
            GrantsCommand::Remove { index: 0 }
        ));
        assert!(
            state_persistence::grants::load_grants(dir.path())
                .grants
                .is_empty()
        );
        assert!(run_grants(Some(dir.path()), GrantsCommand::Clear));
        assert!(!run_grants(None, GrantsCommand::List));
    }
}
