//! The `wave`-mode command denylist: one `~/.wavecode/wave-denylist.json`
//! file owned by the config layer so every session surface (TUI, exec,
//! REPL, ACP, HTTP serve, MCP serve) enforces the same user rules.
//!
//! Entries use the sandbox's `Bash(pattern)` rule syntax; bare entries
//! are scoped to `Bash` by the composition root at load time. A missing
//! or malformed file yields no entries — the same state as a user who
//! never configured the list — and never blocks startup.
//!
//! Like the model catalog, the file lives under the config layer's home
//! directory and is written atomically through [`crate::atomic_file`].

use std::path::{Path, PathBuf};

/// The denylist file name under the wavecode home directory.
const FILE_NAME: &str = "wave-denylist.json";

/// The file shape: one object, one list, room to grow.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct DenylistFile {
    #[serde(default)]
    entries: Vec<String>,
}

/// The denylist file path under `dir` (the wavecode home directory,
/// e.g. `~/.wavecode`).
pub fn path_in(dir: &Path) -> PathBuf {
    dir.join(FILE_NAME)
}

/// The wavecode home directory holding the file, when resolvable and
/// non-empty (`None` mirrors the other stores: no home, no store).
pub fn default_dir() -> Option<PathBuf> {
    let home = crate::home_dir()?;
    if home.as_os_str().is_empty() {
        return None;
    }
    Some(home.join(".wavecode"))
}

/// Load entries from `dir`; a missing or malformed file, or entries
/// that are blank strings, yield an empty list.
pub fn load_from(dir: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path_in(dir)) else {
        return Vec::new();
    };
    serde_json::from_str::<DenylistFile>(&text)
        .map(|file| {
            file.entries
                .into_iter()
                .filter(|entry| !entry.trim().is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Persist `entries` to `dir` (best effort): a sibling temp file is
/// written, flushed to disk, and renamed into place, so a crash
/// mid-write can never leave a truncated file behind. The in-memory
/// list stays authoritative for this process when the write fails.
pub fn save_to(dir: &Path, entries: &[String]) -> std::io::Result<()> {
    let path = path_in(dir);
    std::fs::create_dir_all(dir)?;
    let mut content = serde_json::to_string_pretty(&DenylistFile {
        entries: entries.to_vec(),
    })
    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    content.push('\n');
    crate::atomic_file::write_private_atomic(&path, content.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round trip: saved entries load back verbatim; blank entries are
    /// dropped at load.
    #[test]
    fn save_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        save_to(dir.path(), &["rm -rf".to_string(), "  ".to_string()]).unwrap();
        assert_eq!(load_from(dir.path()), vec!["rm -rf".to_string()]);
    }

    /// A missing or malformed file loads as empty — the same state as a
    /// user who never configured the list — and never panics.
    #[test]
    fn missing_or_malformed_loads_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_from(dir.path()).is_empty());
        std::fs::write(path_in(dir.path()), "{not json").unwrap();
        assert!(load_from(dir.path()).is_empty());
    }

    /// The file lands at `<dir>/wave-denylist.json` as JSON with an
    /// `entries` key (shape lock).
    #[test]
    fn file_shape_is_an_entries_object() {
        let dir = tempfile::tempdir().unwrap();
        save_to(dir.path(), &["git push --force".to_string()]).unwrap();
        let text = std::fs::read_to_string(path_in(dir.path())).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            value,
            serde_json::json!({ "entries": ["git push --force"] })
        );
    }
}
