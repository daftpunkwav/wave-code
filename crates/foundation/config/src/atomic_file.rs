//! Crash-safe owner-only replace for config files under `~/.wavecode`.
//!
//! The config crate stays dependency-free, so this is the one local staging
//! write shared by the model catalog and the wave denylist. Do not copy it
//! again. `infrastructure_base::atomic_write_private` is the same idea one
//! layer down; this crate cannot import it.

use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Per-process staging sequence: concurrent writers in this process never
/// share one temp file, and the process id separates processes.
static STAGING_SEQ: AtomicU64 = AtomicU64::new(0);

/// Write `contents` to `path` via a sibling temp file, an fsync, and a rename.
///
/// On Unix the staging file is created and kept owner-only (`0o600`); the
/// rename carries that mode onto `path`. A failed rename removes the temp
/// file. The caller creates the parent directory and appends any trailing
/// newline before calling.
pub(crate) fn write_private_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension(format!(
        "json.staging-{}-{}",
        std::process::id(),
        STAGING_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    // One write path for every platform: only the open (plus the Unix
    // owner-only mode) differs, while the write and the durability flush
    // must never drift apart between the two branches.
    let mut file = {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            // An existing temp file keeps its mode on rewrite: tighten it
            // to owner-only as well, and the rename carries that mode.
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            file
        }
        #[cfg(not(unix))]
        std::fs::File::create(&tmp)?
    };
    file.write_all(contents)?;
    // Flush to disk before the rename: without it the rename covers
    // placement, not durability.
    file.sync_all()?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}
