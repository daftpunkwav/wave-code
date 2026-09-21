/*!
 * @file Grants
 * @description Persisted "always allow" approvals shared by every session.
 *
 * Responsibilities:
 * - Store one literal rule entry per human grant under the home directory.
 * - Load the table for session assembly, deduplicated, malformed lines
 *   counted rather than hidden.
 * - Revoke a single entry by index, or clear the whole table.
 *
 * This module must not depend on: the sandbox crate or any frontend. Rule
 * syntax (`Scope(pattern)`) is an opaque string here — matching and verdicts
 * stay where the sandbox decides them. It only enforces the one invariant
 * that matters at this boundary: stored entries are literals.
 */

use std::io::Write;
use std::path::{Path, PathBuf};

/// File name under `<home>/.wavecode/`.
const GRANTS_FILE: &str = "grants.jsonl";

/// One persisted approval grant.
///
/// `rule` is the display form of the derived rule (`Bash(cargo test
/// --locked)`), so the table stays hand-readable and the assembly layer can
/// feed it straight into the sandbox's rule parser. `tool` and `session` are
/// provenance for `wavecode grants list` — they never affect matching.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Grant {
    /// Rule entry in `Scope(pattern)` form, pattern guaranteed literal.
    pub rule: String,
    /// Tool the human approved (provenance only).
    pub tool: String,
    /// Unix seconds at grant time.
    pub granted_at_secs: u64,
    /// Session the grant was made in.
    pub session: String,
}

/// Grant store errors.
#[derive(Debug, thiserror::Error)]
pub enum GrantError {
    /// Filesystem failure (unreadable home, denied write).
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A stored pattern containing a wildcard character would re-parse as a
    /// wildcard rule on the next load and silently widen the allow surface.
    #[error("grants must be literal commands or paths, not wildcard patterns: {0}")]
    Wildcard(String),
}

/// Outcome of loading the table.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct GrantsRead {
    /// Grants that parsed, in the order they were written.
    pub grants: Vec<Grant>,
    /// Lines skipped as unreadable.
    pub malformed: usize,
}

/// Where the table lives, for status and doctor output.
pub fn grants_path(home: &Path) -> PathBuf {
    home.join(".wavecode").join(GRANTS_FILE)
}

/// Load the table: missing file is an empty table, blank lines are ignored,
/// an unreadable line costs that line only.
///
/// Duplicates collapse to their first occurrence — an append-only file can
/// hold the same grant twice (a rewrite raced a re-grant), and the allow
/// table downstream would otherwise carry the same rule repeatedly.
pub fn load_grants(home: &Path) -> GrantsRead {
    let text = match std::fs::read_to_string(grants_path(home)) {
        Ok(text) => text,
        Err(_) => return GrantsRead::default(),
    };
    let mut grants = Vec::new();
    let mut malformed = 0usize;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<Grant>(line) {
            Ok(grant) if !grants.iter().any(|seen: &Grant| seen.rule == grant.rule) => {
                grants.push(grant)
            }
            Ok(_) => {}
            Err(_) => malformed += 1,
        }
    }
    GrantsRead { grants, malformed }
}

/// Append one grant; `false` when the same rule is already stored.
///
/// Patterns carrying `*` or `?` are refused: the derivation that produced
/// them compared literally this session, and re-parsing them as config rules
/// on the next load would turn approved literals into a wildcard allow
/// surface. Callers keep the in-session rule and let the human author a
/// wildcard rule in the config file if they want one.
pub fn add_grant(home: &Path, grant: &Grant) -> Result<bool, GrantError> {
    if grant.rule.contains(['*', '?']) {
        return Err(GrantError::Wildcard(grant.rule.clone()));
    }
    if load_grants(home)
        .grants
        .iter()
        .any(|seen| seen.rule == grant.rule)
    {
        return Ok(false);
    }
    let path = grants_path(home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let line = serde_json::to_string(grant)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    writeln!(file, "{line}")?;
    file.flush()?;
    Ok(true)
}

/// Drop the grant at `index` (the order `load_grants` reported). Returns the
/// removed grant, or `None` when the index is out of range.
pub fn remove_grant(home: &Path, index: usize) -> Result<Option<Grant>, GrantError> {
    let read = load_grants(home);
    let Some(removed) = read.grants.get(index).cloned() else {
        return Ok(None);
    };
    rewrite(home, &without(&read.grants, index))?;
    Ok(Some(removed))
}

/// Drop every grant; returns how many were stored.
pub fn clear_grants(home: &Path) -> Result<usize, GrantError> {
    let read = load_grants(home);
    rewrite(home, &[])?;
    Ok(read.grants.len())
}

/// Entries without the one at `index`.
fn without(grants: &[Grant], index: usize) -> Vec<Grant> {
    grants
        .iter()
        .enumerate()
        .filter(|(at, _)| *at != index)
        .map(|(_, grant)| grant.clone())
        .collect()
}

/// Replace the whole table with `grants`.
///
/// Write-then-rename, so a crash mid-rewrite leaves the previous table
/// intact rather than a truncated file that reads back as "no grants" —
/// revoking must never be the thing that silently widens authority.
/// Malformed lines are deliberately not carried over: a rewrite is the
/// repair path for a torn tail.
fn rewrite(home: &Path, grants: &[Grant]) -> Result<(), GrantError> {
    let path = grants_path(home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut text = String::new();
    for grant in grants {
        let line = serde_json::to_string(grant)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        text.push_str(&line);
        text.push('\n');
    }
    let tmp = path.with_extension("jsonl.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(rule: &str) -> Grant {
        Grant {
            rule: rule.to_string(),
            tool: "shell".to_string(),
            granted_at_secs: 100,
            session: "s1".to_string(),
        }
    }

    #[test]
    fn empty_when_no_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_grants(dir.path()), GrantsRead::default());
    }

    #[test]
    fn appends_and_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        assert!(add_grant(dir.path(), &grant("Bash(cargo test)")).unwrap());
        let read = load_grants(dir.path());
        assert_eq!(read.grants.len(), 1);
        assert_eq!(read.grants[0].rule, "Bash(cargo test)");
        assert_eq!(read.malformed, 0);
    }

    /// Re-granting the same rule is a no-op, not a second row.
    #[test]
    fn duplicate_rules_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        assert!(add_grant(dir.path(), &grant("Bash(git status)")).unwrap());
        assert!(!add_grant(dir.path(), &grant("Bash(git status)")).unwrap());
        assert_eq!(load_grants(dir.path()).grants.len(), 1);
    }

    /// The widening guard: a wildcard pattern would re-parse as a glob.
    #[test]
    fn wildcard_patterns_are_not_stored() {
        let dir = tempfile::tempdir().unwrap();
        for rule in ["Bash(git *)", "File(src/**)", "Bash(grep -r foo? .)"] {
            assert!(
                matches!(
                    add_grant(dir.path(), &grant(rule)),
                    Err(GrantError::Wildcard(_))
                ),
                "{rule} must not be persistable"
            );
        }
        assert_eq!(load_grants(dir.path()).grants.len(), 0);
    }

    /// A torn tail line costs that line, not the table.
    #[test]
    fn malformed_lines_are_counted() {
        let dir = tempfile::tempdir().unwrap();
        add_grant(dir.path(), &grant("Bash(ls)")).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(grants_path(dir.path()))
            .and_then(|mut f| writeln!(f, "{{\"rule\":"))
            .unwrap();
        let read = load_grants(dir.path());
        assert_eq!(read.grants.len(), 1);
        assert_eq!(read.malformed, 1);
    }

    #[test]
    fn remove_and_clear_rewrite_the_table() {
        let dir = tempfile::tempdir().unwrap();
        add_grant(dir.path(), &grant("Bash(a)")).unwrap();
        add_grant(dir.path(), &grant("Bash(b)")).unwrap();
        add_grant(dir.path(), &grant("Bash(c)")).unwrap();

        assert_eq!(
            remove_grant(dir.path(), 1).unwrap().map(|g| g.rule),
            Some("Bash(b)".to_string())
        );
        assert!(remove_grant(dir.path(), 9).unwrap().is_none());
        assert_eq!(
            load_grants(dir.path()).grants.len(),
            2,
            "removal keeps the others"
        );
        assert_eq!(clear_grants(dir.path()).unwrap(), 2);
        assert_eq!(load_grants(dir.path()).grants.len(), 0);
    }

    /// Revocation order matches load order, which is what the CLI prints.
    #[test]
    fn removal_by_index_follows_load_order() {
        let dir = tempfile::tempdir().unwrap();
        add_grant(dir.path(), &grant("Bash(first)")).unwrap();
        add_grant(dir.path(), &grant("Bash(second)")).unwrap();
        remove_grant(dir.path(), 0).unwrap();
        let left = load_grants(dir.path()).grants;
        assert_eq!(
            left.iter().map(|g| g.rule.as_str()).collect::<Vec<_>>(),
            ["Bash(second)"]
        );
    }
}
