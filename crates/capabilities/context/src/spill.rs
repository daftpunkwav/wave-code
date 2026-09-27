/*! @file SpillStore
 * @description Side-store for oversized tool outputs plus the pruner stage.
 *
 * Responsibilities:
 * - Persist over-threshold tool outputs outside the working directory
 * - Enforce a total size cap with oldest-first eviction plus a manifest
 * - Expose the pruner entry point (spill + marker + head excerpt)
 *
 * This module must not depend on: runtime, transport, or UI-layer components.
 */

//! Spill side-store: large tool outputs are written to a home-scoped
//! directory and the model sees only a [`PRUNE_MARKER`] plus the `spill://`
//! URI and a head excerpt. Reads go through the `spill_read` tool (tools
//! crate), which resolves the same store root.

use std::path::{Path, PathBuf};

/// Total size cap for the spill store (32 MB); oldest entries evict first.
pub const SPILL_TOTAL_CAP_BYTES: u64 = 32 * 1024 * 1024;

/// Default per-output prune threshold in chars (overridable by callers).
pub const DEFAULT_PRUNE_THRESHOLD_CHARS: usize = 8192;

/// Head excerpt length (chars) kept inline when an output is spilled.
pub const PRUNE_HEAD_CHARS: usize = 1024;

/// Marker prefixing every pruned output so the model can tell spilled text
/// apart from verbatim tool output.
pub const PRUNE_MARKER: &str = "[pruned: output spilled to side-store]";

/// Spill URI scheme: reads resolve `spill://<id>` via the store.
pub const SPILL_SCHEME: &str = "spill://";

/// One manifest entry (serialized as `id bytes created_at` lines).
#[derive(Debug, Clone, PartialEq, Eq)]
struct ManifestEntry {
    id: String,
    bytes: u64,
    created_at: u64,
}

fn parse_manifest(raw: &str) -> Vec<ManifestEntry> {
    raw.lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let id = parts.next()?.to_owned();
            let bytes = parts.next()?.parse().ok()?;
            let created_at = parts.next()?.parse().ok()?;
            if validate_spill_id(&id).is_err() {
                return None;
            }
            Some(ManifestEntry {
                id,
                bytes,
                created_at,
            })
        })
        .collect()
}

fn render_manifest(entries: &[ManifestEntry]) -> String {
    entries
        .iter()
        .map(|e| format!("{} {} {}", e.id, e.bytes, e.created_at))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Spill error type.
#[derive(Debug, thiserror::Error)]
pub enum SpillError {
    /// Filesystem failure.
    #[error("spill IO failed: {0}")]
    Io(String),
    /// Unknown or malformed spill URI.
    #[error("unknown spill: {0}")]
    Unknown(String),
}

impl From<std::io::Error> for SpillError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

impl std::fmt::Display for ManifestEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({} bytes)", self.id, self.bytes)
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Validate a spill id: `[A-Za-z0-9_-]{1,64}` (manifest/file-name safe).
pub fn validate_spill_id(id: &str) -> std::result::Result<(), String> {
    if id.is_empty() || id.len() > 64 {
        return Err(format!(
            "invalid spill id {id:?}: expected 1-64 chars ([A-Za-z0-9_-])"
        ));
    }
    if id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        Ok(())
    } else {
        Err(format!(
            "invalid spill id {id:?}: expected [A-Za-z0-9_-] only"
        ))
    }
}

/// Parse a `spill://<id>` URI into its id.
pub fn parse_spill_uri(uri: &str) -> std::result::Result<String, SpillError> {
    let id = uri
        .strip_prefix(SPILL_SCHEME)
        .ok_or_else(|| SpillError::Unknown(uri.to_owned()))?;
    validate_spill_id(id).map_err(SpillError::Unknown)?;
    Ok(id.to_owned())
}

/// User home directory, delegating to the shared [`wavecode_config::
/// home_dir`] definition (`USERPROFILE` first, `HOME` fallback).
pub fn spill_home_dir() -> Option<PathBuf> {
    wavecode_config::home_dir()
}

/// Effective store root: `<home>/.wavecode/spills`, falling back to the
/// system temp dir when home cannot be resolved (explicit, never cwd).
pub fn default_spill_store_root() -> PathBuf {
    spill_home_dir()
        .map(|h| h.join(".wavecode").join("spills"))
        .unwrap_or_else(|| std::env::temp_dir().join("wavecode-spills"))
}

/// Home-scoped side-store for oversized tool outputs with oldest-eviction.
#[derive(Debug)]
pub struct SpillStore {
    /// Serializes manifest read-modify-write cycles within this process.
    /// (Cross-process writers share the root and stay racy; the atomic
    /// save bounds that damage to a lost update, never a truncated
    /// ledger.)
    lock: std::sync::Mutex<()>,
    root: PathBuf,
}

impl SpillStore {
    /// Build with an explicit root (tests inject a tempdir; production uses
    /// [`default_spill_store_root`]).
    pub fn new(root: PathBuf) -> Self {
        Self {
            lock: std::sync::Mutex::new(()),
            root,
        }
    }

    /// Store root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn manifest_path(&self) -> PathBuf {
        self.root.join("manifest.txt")
    }

    fn entry_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.txt"))
    }

    fn load_manifest(&self) -> Vec<ManifestEntry> {
        let raw = std::fs::read_to_string(self.manifest_path()).unwrap_or_default();
        parse_manifest(&raw)
    }

    fn save_manifest(&self, entries: &[ManifestEntry]) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.root)?;
        // The manifest is the sole ledger for the 32MB cap: a truncated
        // write would read back as "0 bytes used" and disable eviction
        // forever, so it lands temp+rename (per docs/defensive-patterns.md).
        let staging = self.root.join("manifest.txt.staging-tmp");
        std::fs::write(&staging, render_manifest(entries))?;
        if let Err(e) = std::fs::rename(&staging, self.manifest_path()) {
            let _ = std::fs::remove_file(&staging);
            return Err(e);
        }
        Ok(())
    }

    /// Spill `content` into the store, returning its `spill://` URI.
    /// Enforces [`SPILL_TOTAL_CAP_BYTES`] with oldest-first eviction.
    pub fn spill(&self, content: &str) -> std::result::Result<String, SpillError> {
        std::fs::create_dir_all(&self.root)?;
        let id = Self::new_id();
        validate_spill_id(&id).map_err(SpillError::Unknown)?;
        let bytes = content.as_bytes();
        // The lock spans the whole read-modify-write so concurrent spills
        // (parallel tool rounds) cannot lose entries.
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        std::fs::write(self.entry_path(&id), bytes)?;
        let mut entries = self.load_manifest();
        entries.push(ManifestEntry {
            id: id.clone(),
            bytes: bytes.len() as u64,
            created_at: now_secs(),
        });
        self.evict_to_cap(&mut entries);
        self.save_manifest(&entries)?;
        Ok(format!("{SPILL_SCHEME}{id}"))
    }

    /// Read a spilled output by `spill://` URI.
    pub fn read(&self, uri: &str) -> std::result::Result<String, SpillError> {
        let id = parse_spill_uri(uri)?;
        let raw =
            std::fs::read(self.entry_path(&id)).map_err(|_| SpillError::Unknown(uri.to_owned()))?;
        String::from_utf8(raw).map_err(|_| SpillError::Unknown(uri.to_owned()))
    }

    fn new_id() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        format!("{:x}-{}-{}", now_secs(), std::process::id(), n)
    }

    /// Drop oldest entries until the total fits the cap (files and manifest
    /// pruned together; a missing file counts its manifest bytes anyway).
    fn evict_to_cap(&self, entries: &mut Vec<ManifestEntry>) {
        let mut total: u64 = entries.iter().map(|e| e.bytes).sum();
        while total > SPILL_TOTAL_CAP_BYTES && !entries.is_empty() {
            let oldest = entries.remove(0);
            total = total.saturating_sub(oldest.bytes);
            let _ = std::fs::remove_file(self.entry_path(&oldest.id));
        }
    }
}

/// Prune one tool output: outputs at or under `threshold_chars` pass through
/// untouched; larger ones spill to `store` and the caller sees
/// [`PRUNE_MARKER`] plus the `spill://` URI, the total char count, and a head
/// excerpt of [`PRUNE_HEAD_CHARS`] chars.
pub fn prune_tool_output(content: &str, store: &SpillStore, threshold_chars: usize) -> String {
    let total = content.chars().count();
    if total <= threshold_chars {
        return content.to_owned();
    }
    let uri = match store.spill(content) {
        Ok(uri) => uri,
        Err(e) => return format!("{PRUNE_MARKER}\n(spill failed: {e})\n\n{content}"),
    };
    let head: String = content.chars().take(PRUNE_HEAD_CHARS).collect();
    format!("{PRUNE_MARKER}\nspill: {uri}\nchars: {total}\nhead:\n{head}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spill_and_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = SpillStore::new(dir.path().to_path_buf());
        let uri = store.spill("hello spill").unwrap();
        assert!(uri.starts_with(SPILL_SCHEME));
        assert_eq!(store.read(&uri).unwrap(), "hello spill");
    }

    #[test]
    fn read_rejects_bad_uris() {
        let dir = tempfile::tempdir().unwrap();
        let store = SpillStore::new(dir.path().to_path_buf());
        assert!(store.read("spill://../evil").is_err());
        assert!(store.read("file:///etc/passwd").is_err());
        assert!(store.read("spill://missing-id").is_err());
    }

    #[test]
    fn prune_passes_small_outputs_through() {
        let dir = tempfile::tempdir().unwrap();
        let store = SpillStore::new(dir.path().to_path_buf());
        let out = prune_tool_output("small", &store, DEFAULT_PRUNE_THRESHOLD_CHARS);
        assert_eq!(out, "small");
        // Nothing spilled for small outputs.
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    }

    #[test]
    fn prune_spills_large_outputs_with_marker_and_head() {
        let dir = tempfile::tempdir().unwrap();
        let store = SpillStore::new(dir.path().to_path_buf());
        let big: String = (0..9000).map(|i| format!("line{i}\n")).collect();
        let out = prune_tool_output(&big, &store, DEFAULT_PRUNE_THRESHOLD_CHARS);
        assert!(out.starts_with(PRUNE_MARKER));
        assert!(out.contains("spill://"));
        assert!(out.contains("chars: "));
        // Head excerpt present, full body not inline.
        assert!(out.contains("line0"));
        assert!(!out.contains("line8999"));
        // Readback returns the full output.
        let uri = out
            .lines()
            .find_map(|l| l.strip_prefix("spill: "))
            .expect("spill URI line")
            .trim()
            .to_owned();
        assert_eq!(store.read(&uri).unwrap(), big);
    }

    #[test]
    fn threshold_is_config_overridable() {
        let dir = tempfile::tempdir().unwrap();
        let store = SpillStore::new(dir.path().to_path_buf());
        let content = "x".repeat(100);
        assert_eq!(prune_tool_output(&content, &store, 200), content);
        let pruned = prune_tool_output(&content, &store, 10);
        assert!(pruned.starts_with(PRUNE_MARKER));
    }

    #[test]
    fn manifest_survives_corruption_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = SpillStore::new(dir.path().to_path_buf());
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(dir.path().join("manifest.txt"), "{corrupt").unwrap();
        let uri = store.spill("still works").unwrap();
        assert_eq!(store.read(&uri).unwrap(), "still works");
    }
}
