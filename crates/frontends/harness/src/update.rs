//! Update check and self-install: compare the running version against
//! the newest published GitHub release on the binary-release repo,
//! and optionally replace the running binary with it.
//!
//! Downloads and installs happen only on the explicit surface
//! (`wavecode update --install`): the interactive session may run one
//! passive availability check in the background to fill the footer
//! notice, but it never downloads. A check failure is reported and
//! exits non-zero instead of ever degrading into a false "up to date".
//! The install path downloads the bare release asset from the GitHub
//! host only (origin allowlist in [`fetch_release`]), verifies it
//! against the published sha256 checksum before touching anything,
//! refuses to touch source builds, and keeps a `.bak` of the replaced
//! binary as the rollback copy.
//!
//! Accepted posture: the sha256 ships as an asset of the same release,
//! so the trust root is the HTTPS github.com channel plus this origin
//! allowlist — there is no out-of-band signature chain. That guards the
//! download against tampering in transit and against a misbehaving API
//! payload, not against a compromised release pipeline; if that threat
//! model changes, signing (e.g. minisign/cosign) is the upgrade path.

use anyhow::{Context as _, Result};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

/// Repository whose GitHub Releases carry `wavecode` binaries.
const RELEASES_REPO: &str = "daftpunkwav/wave-code";

/// The only host a release asset may download from. `browser_download_url`
/// values on GitHub Releases point here; anything else in the API payload
/// is dropped at parse time. The sha256 verification stays the
/// content-level guarantee, this is the origin-level one: a tampered or
/// misbehaving API response must not redirect the binary download (and
/// its checksum) to an unknown host.
const RELEASE_ASSET_HOST: &str = "github.com";

/// True when `url` is an https URL on [`RELEASE_ASSET_HOST`] (case- and
/// trailing-dot-insensitive on the host, the forms reqwest compares
/// case-insensitively too). Subdomain lookalikes
/// (`github.com.evil.test`) and http-downgrades fail the check.
fn is_release_asset_url(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    if parsed.scheme() != "https" {
        return false;
    }
    match parsed.host_str() {
        // A trailing root dot is the DNS root label, still github.com.
        Some(host) => host
            .strip_suffix('.')
            .unwrap_or(host)
            .eq_ignore_ascii_case(RELEASE_ASSET_HOST),
        None => false,
    }
}

/// One downloadable file attached to a release.
#[derive(Debug, Clone)]
pub struct Asset {
    /// File name as published (e.g. `wavecode-0.2.0-x86_64-...-bin.exe`).
    pub name: String,
    /// Browser download URL.
    pub url: String,
}

/// Result of comparing the running version to the latest release.
pub enum UpdateStatus {
    /// Running version is the newest published release.
    UpToDate { latest: String },
    /// A newer release exists; `url` points at the release page.
    Available { latest: String, url: String },
}

/// Numeric dotted-version comparison: `0.2.1` > `0.2.0` > `0.2`.
/// Missing segments compare as zero, so short forms never trigger a
/// false update; non-numeric segments (prerelease suffixes) also
/// compare as zero and therefore never claim to be newer.
pub fn version_is_newer(latest: &str, current: &str) -> bool {
    let segments = |version: &str| -> Vec<u64> {
        version
            .trim()
            .trim_start_matches('v')
            .split('.')
            .map(|part| part.trim().parse().unwrap_or(0))
            .collect()
    };
    let latest_segments = segments(latest);
    let current_segments = segments(current);
    for index in 0..latest_segments.len().max(current_segments.len()) {
        let latest_value = latest_segments.get(index).copied().unwrap_or(0);
        let current_value = current_segments.get(index).copied().unwrap_or(0);
        if latest_value != current_value {
            return latest_value > current_value;
        }
    }
    false
}

/// Fetch the newest published release as `(tag_name, html_url)`.
/// `Ok(None)` means the repo has no published release yet (404).
pub async fn fetch_latest(client: &reqwest::Client) -> Result<Option<(String, String)>> {
    Ok(fetch_release(client)
        .await?
        .map(|release| (release.tag, release.page)))
}

/// The newest published release with its downloadable assets.
pub struct Release {
    /// Release tag (e.g. `v0.2.1`).
    pub tag: String,
    /// Release page URL.
    pub page: String,
    /// Attached files.
    pub assets: Vec<Asset>,
}

/// Fetch the newest published release with its assets. `Ok(None)`
/// means the repo has no published release yet (404).
pub async fn fetch_release(client: &reqwest::Client) -> Result<Option<Release>> {
    let url = format!("https://api.github.com/repos/{RELEASES_REPO}/releases/latest");
    let response = client
        .get(url)
        .header(
            reqwest::header::USER_AGENT,
            concat!("wavecode/", env!("CARGO_PKG_VERSION")),
        )
        .send()
        .await
        .context("request to GitHub releases failed")?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let payload: Value = response
        .error_for_status()
        .context("GitHub releases API returned an error")?
        .json()
        .await
        .context("GitHub releases API returned non-JSON")?;
    let tag = payload["tag_name"]
        .as_str()
        .context("release payload missing tag_name")?
        .to_string();
    let page = payload["html_url"].as_str().unwrap_or_default().to_string();
    let mut assets = Vec::new();
    if let Some(list) = payload["assets"].as_array() {
        for asset in list {
            let name = asset["name"].as_str().unwrap_or_default();
            let asset_url = asset["browser_download_url"].as_str().unwrap_or_default();
            if !name.is_empty() && !asset_url.is_empty() && is_release_asset_url(asset_url) {
                assets.push(Asset {
                    name: name.to_string(),
                    url: asset_url.to_string(),
                });
            }
        }
    }
    Ok(Some(Release { tag, page, assets }))
}

/// Classify the running version against the fetched release tag.
pub fn classify(latest: &str, url: &str) -> UpdateStatus {
    if version_is_newer(latest, env!("CARGO_PKG_VERSION")) {
        UpdateStatus::Available {
            latest: latest.to_string(),
            url: url.to_string(),
        }
    } else {
        UpdateStatus::UpToDate {
            latest: latest.to_string(),
        }
    }
}

/// The release target this binary was built for, matching the
/// release.yml matrix; `None` on targets the release pipeline does
/// not publish (installers must fall back to "build from source").
pub fn current_target() -> Option<&'static str> {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        Some("x86_64-unknown-linux-gnu")
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        Some("aarch64-apple-darwin")
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        Some("x86_64-apple-darwin")
    }
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        Some("x86_64-pc-windows-msvc")
    }
    #[cfg(not(any(
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_os = "macos", target_arch = "aarch64"),
        all(target_os = "macos", target_arch = "x86_64"),
        all(target_os = "windows", target_arch = "x86_64")
    )))]
    {
        None
    }
}

/// True when the running binary came from a cargo build (`target/`
/// in its path): such a binary has no installed copy to replace, so
/// self-update must refuse instead of clobbering a build artifact.
pub fn running_from_source(exe: &std::path::Path) -> bool {
    let mut components = exe.components();
    while let Some(part) = components.next() {
        if part.as_os_str() == "target"
            && let Some(next) = components.next()
        {
            let dir = next.as_os_str().to_string_lossy();
            if dir == "debug" || dir == "release" {
                return true;
            }
        }
    }
    false
}

/// The bare-binary asset name for one release (`release.yml` publishes
/// `wavecode-{version}-{target}-bin{exe}` plus its `.sha256`).
pub fn asset_name(target: &str, version: &str) -> String {
    let exe = if cfg!(windows) { ".exe" } else { "" };
    format!("wavecode-{version}-{target}-bin{exe}")
}

/// Find the bare binary and its checksum file among a release's
/// assets. The tag's `v` prefix is dropped for the asset version.
pub fn pick_assets<'a>(
    release_tag: &str,
    assets: &'a [Asset],
    target: &str,
) -> Option<(&'a Asset, &'a Asset)> {
    let version = release_tag.trim_start_matches('v');
    let binary = asset_name(target, version);
    let checksum = format!("{binary}.sha256");
    let bin = assets.iter().find(|a| a.name == binary)?;
    let sha = assets.iter().find(|a| a.name == checksum)?;
    Some((bin, sha))
}

/// Check `bytes` against the contents of a published `.sha256` file
/// (`"<hex>  <filename>"`; only the hex token is compared).
pub fn verify_sha256(bytes: &[u8], checksum_file: &str) -> Result<()> {
    let expected = checksum_file
        .split_whitespace()
        .next()
        .context("checksum file is empty")?
        .to_lowercase();
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let actual: String = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if actual != expected {
        anyhow::bail!("checksum mismatch: expected {expected}, got {actual}");
    }
    Ok(())
}

/// Replace the running binary with `bytes`, keeping the old file as
/// `<name>.bak` for rollback. The download lands in a staging file
/// next to the target (same filesystem, so the final step is a
/// rename); verification happened before this is called.
pub fn apply_swap(target_exe: &std::path::Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    let file_name = target_exe
        .file_name()
        .context("binary path has no file name")?
        .to_string_lossy()
        .into_owned();
    let dir = target_exe.parent().context("binary path has no parent")?;
    let staging = dir.join(format!(".{file_name}.download-{}", std::process::id()));
    {
        let mut file = std::fs::File::create(&staging)
            .with_context(|| format!("creating {}", staging.display()))?;
        guarded_staging(&staging, || -> Result<()> {
            file.write_all(bytes)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                // Mirror the installed binary's mode so the replacement
                // neither widens nor drops access; fall back to
                // owner-only when the old mode cannot be read.
                let permissions = std::fs::metadata(target_exe)
                    .map(|meta| meta.permissions())
                    .unwrap_or_else(|_| std::fs::Permissions::from_mode(0o700));
                // Strip any ACL the staging file inherited from its
                // directory before the mode mirror lands: stripping an
                // ACL re-derives the mode, and the mirror must be the
                // last word on it.
                clear_staging_acl(&staging)?;
                std::fs::set_permissions(&staging, permissions)?;
                // ACLs are invisible to `std::fs`; they must land after
                // the mode mirror, because writing the ACL re-derives
                // the mode, while a later chmod would rewrite (and
                // could drop) the copied entries. The copy is a
                // snapshot taken here, not a lock: an administrator
                // changing the target's ACL between this probe and the
                // rename is overwritten — the same accepted window the
                // mode mirror already has.
                copy_acl(target_exe, &staging)?;
            }
            // The final fsync covers the content, the mode mirror, and
            // the ACL work together: the rename below is only as
            // durable as what it names.
            file.sync_all()?;
            Ok(())
        })?;
    }
    // Step the old binary aside as `.bak`, then move the staging file
    // into its place. Windows cannot delete a running exe, but it can
    // rename one; a failed second step rolls the `.bak` back. A previous
    // update leaves `.bak` behind, and Windows refuses to rename onto an
    // existing file, so the old rollback copy is removed first.
    let backup = dir.join(format!("{file_name}.bak"));
    let _ = std::fs::remove_file(&backup);
    if let Err(error) = std::fs::rename(target_exe, &backup) {
        let _ = std::fs::remove_file(&staging);
        return Err(error)
            .with_context(|| format!("moving the old binary to {}", backup.display()));
    }
    if let Err(error) = std::fs::rename(&staging, target_exe) {
        let _ = std::fs::rename(&backup, target_exe);
        let _ = std::fs::remove_file(&staging);
        return Err(error).context("install failed; the old binary was restored");
    }
    Ok(())
}

/// Strip any ACL the staged file picked up from its parent directory
/// before the mode mirror lands. A directory can hand a freshly created
/// file entries the installed binary never carried (Linux default ACLs
/// materialize on every create; macOS `file_inherit` entries do the
/// same), and the rename would install those grants on the replacement.
#[cfg(unix)]
fn clear_staging_acl(staging: &std::path::Path) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        clear_linux_acl(staging)
    }

    #[cfg(target_os = "macos")]
    {
        clear_macos_acl(staging)
    }

    // Other unix targets ship neither a release binary nor ACL support
    // in this workspace; there is nothing to strip.
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = staging;
        Ok(())
    }
}

/// Run the staging mutations under one guard: a failure at any step
/// removes the partial download before the error surfaces, so a failed
/// update never leaves debris next to the target. The rename-step
/// errors clean up in their own arms inside [`apply_swap`].
fn guarded_staging(staging: &std::path::Path, stage: impl FnOnce() -> Result<()>) -> Result<()> {
    let outcome = stage();
    if outcome.is_err() {
        let _ = std::fs::remove_file(staging);
    }
    outcome
}

/// Carry a POSIX ACL from the installed binary onto the staged
/// replacement. Returns `false` when the target has no ACL and `true`
/// when one was copied; an error aborts the swap, so the update never
/// installs a replacement that silently drops an access grant. The
/// read is a snapshot taken at swap time — an administrator changing
/// the target's ACL between this probe and the rename is overwritten,
/// the same accepted window the mode mirror already has.
#[cfg(unix)]
fn copy_acl(target_exe: &std::path::Path, staging: &std::path::Path) -> Result<bool> {
    #[cfg(target_os = "linux")]
    {
        copy_linux_acl(target_exe, staging)
    }

    #[cfg(target_os = "macos")]
    {
        copy_macos_acl(target_exe, staging)
    }

    // Other unix targets ship neither a release binary nor ACL support
    // in this workspace; the mode mirror remains the base behavior.
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (target_exe, staging);
        Ok(false)
    }
}

/// True for the errors that mean "this filesystem keeps no POSIX ACL
/// xattrs at all" (`EOPNOTSUPP`, which std surfaces as
/// [`std::io::ErrorKind::Unsupported`]): where the kernel stores no
/// ACLs, a probe failure cannot be concealing a grant to preserve.
#[cfg(target_os = "linux")]
fn acl_xattrs_unsupported(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::Unsupported
}

/// Linux: strip any access ACL the staged file inherited from its
/// parent directory. A default ACL on the directory materializes an
/// access ACL on every file created inside it, including this one.
#[cfg(target_os = "linux")]
fn clear_linux_acl(staging: &std::path::Path) -> Result<()> {
    const ACCESS_ACL: &str = "system.posix_acl_access";
    match xattr::remove(staging, ACCESS_ACL) {
        Ok(()) => Ok(()),
        // Nothing inherited — the usual path.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        // A filesystem that keeps no ACL xattrs cannot have handed the
        // staging file one either.
        Err(error) if acl_xattrs_unsupported(&error) => Ok(()),
        Err(error) => Err(error).context("stripping the staged file's inherited access ACL"),
    }
}

/// Linux: the access ACL lives in a kernel-managed xattr, so preserving
/// it is a verbatim blob copy — no libacl parsing involved. ACLs the
/// mode bits already express are never materialized as an xattr (the
/// kernel folds them into the mode), so an absent attribute means
/// there is nothing beyond the mode to carry over.
#[cfg(target_os = "linux")]
fn copy_linux_acl(target_exe: &std::path::Path, staging: &std::path::Path) -> Result<bool> {
    const ACCESS_ACL: &str = "system.posix_acl_access";
    match xattr::get(target_exe, ACCESS_ACL) {
        Ok(Some(blob)) => {
            xattr::set(staging, ACCESS_ACL, &blob)
                .context("copying the installed binary's access ACL onto the staged file")?;
            Ok(true)
        }
        Ok(None) => Ok(false),
        // A filesystem that keeps no ACL xattrs cannot carry a grant to
        // preserve; the mode mirror remains the whole story.
        Err(error) if acl_xattrs_unsupported(&error) => Ok(false),
        Err(error) => Err(error).context("probing the installed binary's access ACL"),
    }
}

/// macOS: strip any ACL the staged file inherited from its directory.
/// A directory whose ACL entries carry `file_inherit` passes them to
/// every file created inside it, including this one. The strip probe
/// stays at face value: this is a best-effort cleanup on a path that
/// usually carries nothing, so a NULL there means there is nothing to
/// remove.
#[cfg(target_os = "macos")]
fn clear_macos_acl(staging: &std::path::Path) -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    let staged =
        CString::new(staging.as_os_str().as_bytes()).context("staging path has a NUL byte")?;
    // SAFETY: `staged` is a NUL-terminated C string built above; the
    // returned handle is either valid or NULL, and it is released on
    // every path below.
    // nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
    let acl = unsafe { acl_ffi::acl_get_file(staged.as_ptr(), acl_ffi::ACL_TYPE_EXTENDED) };
    if acl.is_null() {
        return Ok(());
    }
    // SAFETY: `acl` is a handle this function owns.
    // nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
    let _inspected = unsafe { acl_ffi::acl_free(acl) };
    // Setting an empty ACL is Darwin's programmatic equivalent of
    // `chmod -N`: the entries go away and the file reports no ACL.
    // SAFETY: `acl_init(0)` returns either a valid empty handle or
    // NULL, and the handle is released on every path below.
    // nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
    let empty = unsafe { acl_ffi::acl_init(0) };
    if empty.is_null() {
        return Err(std::io::Error::last_os_error())
            .context("allocating an empty ACL to strip the staged file");
    }
    // SAFETY: `staged` is a valid C string and `empty` is a valid owned
    // handle.
    let cleared =
        // nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
        unsafe { acl_ffi::acl_set_file(staged.as_ptr(), acl_ffi::ACL_TYPE_EXTENDED, empty) };
    // SAFETY: `empty` is owned and still valid; freeing after the set
    // call cannot affect the staged file.
    // nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
    let _released = unsafe { acl_ffi::acl_free(empty) };
    if cleared != 0 {
        Err(std::io::Error::last_os_error()).context("stripping the staged file's inherited ACL")
    } else {
        Ok(())
    }
}

/// macOS: extended ACLs are not xattrs; libSystem hands them out as
/// opaque `acl_t` handles. The copy stays handle-level — entries are
/// never parsed — and `ACL_TYPE_EXTENDED` is the ACL type Darwin keeps
/// for ordinary files.
#[cfg(target_os = "macos")]
fn copy_macos_acl(target_exe: &std::path::Path, staging: &std::path::Path) -> Result<bool> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    let target =
        CString::new(target_exe.as_os_str().as_bytes()).context("target path has a NUL byte")?;
    let staged =
        CString::new(staging.as_os_str().as_bytes()).context("staging path has a NUL byte")?;
    // Darwin returns NULL both when the file carries no ACL and on
    // probe failures, and the documented `ENOATTR` does not reliably
    // survive on the absent path — the observed value is whatever the
    // last failed syscall left behind. errno is snapshotted around the
    // call, so the NULL is judged by what changed: an errno this call
    // actually set to anything other than `ENOATTR` is a probe failure
    // and aborts the swap instead of silently dropping a real ACL,
    // while an untouched errno keeps the observed face-value reading of
    // "no ACL". An error that lands on the stale value by coincidence
    // is read as absent — the same residual as taking the NULL at face
    // value, but strictly narrower.
    let before = std::io::Error::last_os_error().raw_os_error();
    // SAFETY: both arguments are NUL-terminated C strings built above;
    // the returned handle is either valid or NULL, and it is released on
    // every path below.
    // nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
    let acl = unsafe { acl_ffi::acl_get_file(target.as_ptr(), acl_ffi::ACL_TYPE_EXTENDED) };
    if acl.is_null() {
        let after = std::io::Error::last_os_error().raw_os_error();
        if after == before || after == Some(libc::ENOATTR) {
            return Ok(false);
        }
        return Err(std::io::Error::last_os_error()).context("probing the installed binary's ACL");
    }
    // SAFETY: `acl` is a handle this function owns and `staged` is a
    // valid C string.
    // nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
    let copied = unsafe { acl_ffi::acl_set_file(staged.as_ptr(), acl_ffi::ACL_TYPE_EXTENDED, acl) };
    // SAFETY: `acl` is owned and still valid; freeing after the set call
    // cannot affect the staged file. `acl_free` has no recoverable error
    // on a valid handle.
    // nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
    let _released = unsafe { acl_ffi::acl_free(acl) };
    if copied != 0 {
        Err(std::io::Error::last_os_error())
            .context("copying the installed binary's ACL onto the staged file")
    } else {
        Ok(true)
    }
}

/// The libSystem ACL API is stable but not exposed by the `libc` crate;
/// the handles stay opaque, so the FFI surface is four calls plus the
/// `ACL_TYPE_EXTENDED` constant from `sys/acl.h`. The `acl_size` and
/// `acl_copy_ext` pair exists only so tests can compare ACLs as bytes.
#[cfg(target_os = "macos")]
mod acl_ffi {
    pub type Acl = *mut libc::c_void;

    pub const ACL_TYPE_EXTENDED: libc::c_int = 0x0000_0100;

    unsafe extern "C" {
        pub fn acl_get_file(path: *const libc::c_char, acl_type: libc::c_int) -> Acl;
        pub fn acl_init(count: libc::c_int) -> Acl;
        pub fn acl_set_file(
            path: *const libc::c_char,
            acl_type: libc::c_int,
            acl: Acl,
        ) -> libc::c_int;
        pub fn acl_free(acl: Acl) -> libc::c_int;
        #[cfg(test)]
        pub fn acl_size(acl: Acl) -> libc::ssize_t;
        #[cfg(test)]
        pub fn acl_copy_ext(
            buf_out: *mut libc::c_void,
            acl: Acl,
            size: libc::ssize_t,
        ) -> libc::ssize_t;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn newer_patch_and_minor_detections() {
        assert!(version_is_newer("v0.1.1", "0.1.0"));
        assert!(version_is_newer("0.2.0", "0.1.9"));
        assert!(version_is_newer("1.0", "0.9.9"));
    }

    #[test]
    fn equal_or_older_never_reports_update() {
        assert!(!version_is_newer("v0.1.0", "0.1.0"));
        assert!(!version_is_newer("0.1", "0.1.0"));
        assert!(!version_is_newer("0.0.9", "0.1.0"));
    }

    #[test]
    fn malformed_segments_compare_as_zero() {
        // A prerelease suffix never claims to be newer than its plain
        // base version; garbage parses as zero and loses to any real
        // number.
        assert!(!version_is_newer("0.1.0-rc1", "0.1.0"));
        assert!(version_is_newer("v0.2.0-rc1", "0.1.9"));
        assert!(!version_is_newer("banana", "0.0.1"));
    }

    #[test]
    fn classify_routes_on_comparison() {
        assert!(matches!(
            classify("v0.0.1", "https://example.com/old"),
            UpdateStatus::UpToDate { .. }
        ));
        assert!(matches!(
            classify("v9.9.9", "https://example.com/new"),
            UpdateStatus::Available { .. }
        ));
    }

    /// The asset origin allowlist: GitHub release URLs pass; lookalike
    /// hosts, http downgrades, subdomain spoofs, and garbage fail.
    #[test]
    fn asset_urls_must_point_at_the_release_host() {
        assert!(is_release_asset_url(
            "https://github.com/daftpunkwav/wave-code/releases/download/v0.2.1/wavecode-bin"
        ));
        // Case-insensitive host, trailing root dot tolerated.
        assert!(is_release_asset_url("https://GitHub.com/a/b"));
        assert!(is_release_asset_url("https://github.com./a/b"));
        for rejected in [
            "http://github.com/daftpunkwav/wave-code/releases/download/v/x",
            "https://github.com.evil.test/a/b",
            "https://evil.test/github.com/a/b",
            "https://raw.githubusercontent.com/a/b",
            "ftp://github.com/a/b",
            "not a url",
            "",
        ] {
            assert!(
                !is_release_asset_url(rejected),
                "'{rejected}' must be rejected"
            );
        }
    }

    #[test]
    fn source_builds_are_detected_by_their_path() {
        let source = std::path::Path::new("/repo/target/debug/wavecode");
        // A backslash path is only a `target/release` pair on Windows.
        // Elsewhere it is one component, so the release case uses the
        // host separator.
        #[cfg(windows)]
        let source_release = std::path::Path::new(r"D:\repo\target\release\wavecode.exe");
        #[cfg(not(windows))]
        let source_release = std::path::Path::new("/repo/target/release/wavecode");
        let installed = std::path::Path::new("/home/u/.cargo/bin/wavecode");
        assert!(running_from_source(source));
        assert!(running_from_source(source_release));
        assert!(!running_from_source(installed));
        // A directory merely named "targetdir" must not match: the
        // check looks for the exact `target/{debug,release}` pair.
        let lookalike = std::path::Path::new("/repo/targetdir/debug/wavecode");
        assert!(!running_from_source(lookalike));
    }

    #[test]
    fn asset_names_follow_the_release_layout() {
        let windows = if cfg!(windows) { ".exe" } else { "" };
        assert_eq!(
            asset_name("x86_64-pc-windows-msvc", "0.2.1"),
            format!("wavecode-0.2.1-x86_64-pc-windows-msvc-bin{windows}")
        );
        let assets = vec![
            Asset {
                name: format!("wavecode-0.2.1-x86_64-pc-windows-msvc-bin{windows}"),
                url: "https://example.com/bin".into(),
            },
            Asset {
                name: format!("wavecode-0.2.1-x86_64-pc-windows-msvc-bin{windows}.sha256"),
                url: "https://example.com/bin.sha256".into(),
            },
        ];
        let (bin, sha) =
            pick_assets("v0.2.1", &assets, "x86_64-pc-windows-msvc").expect("both present");
        assert_eq!(bin.name, asset_name("x86_64-pc-windows-msvc", "0.2.1"));
        assert_eq!(sha.name, format!("{}.sha256", bin.name));
        // A tag whose asset set is missing the checksum file picks
        // nothing rather than installing unverified bytes.
        let only_binary = vec![assets[0].clone()];
        assert!(pick_assets("v0.2.1", &only_binary, "x86_64-pc-windows-msvc").is_none());
    }

    #[test]
    fn checksum_verification_accepts_exact_bytes_only() {
        // sha256("abc") — the canonical test vector.
        let checksum =
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  wavecode-bin\n";
        verify_sha256(b"abc", checksum).expect("matching bytes pass");
        let error = verify_sha256(b"abd", checksum).unwrap_err().to_string();
        assert!(error.contains("checksum mismatch"), "{error}");
        assert!(verify_sha256(b"abc", "").is_err(), "empty checksum file");
    }

    #[test]
    fn swap_replaces_the_binary_and_keeps_a_rollback_copy() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wavecode.exe");
        std::fs::write(&target, b"old-binary-bytes").unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o750)).unwrap();
        apply_swap(&target, b"new-binary-bytes").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new-binary-bytes");
        #[cfg(unix)]
        {
            let mode = std::fs::metadata(&target).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o750, "the swap keeps the installed mode");
        }
        // The old bytes survive as `.bak` next to the target.
        let backup = dir.path().join("wavecode.exe.bak");
        assert_eq!(std::fs::read(&backup).unwrap(), b"old-binary-bytes");
        // A second install must replace the leftover `.bak`. Windows
        // refuses to rename onto an existing file; Unix rename would
        // have replaced it. Both platforms now drop the old copy first.
        apply_swap(&target, b"newer-binary-bytes").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"newer-binary-bytes");
        assert_eq!(std::fs::read(&backup).unwrap(), b"new-binary-bytes");
        // No staging leftovers: the download file was renamed away.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".download-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    /// Canonical extended access ACL: `USER_OBJ rwx`, named `USER`
    /// (uid 0, `r-x`) as the grant beyond the mode bits, `GROUP_OBJ
    /// r-x`, `MASK r-x`, `OTHER none`. Tags are the Linux uapi
    /// bit-flag values (`0x01`/`0x02`/`0x04`/`0x10`/`0x20` — not the
    /// sequential POSIX.1e draft numbering), records are little-endian
    /// (tag u16, perm u16, id u32) after a 4-byte
    /// `POSIX_ACL_XATTR_VERSION = 2` header, and object entries carry
    /// `ACL_UNDEFINED_ID`. The named-user entry keeps the ACL
    /// non-equivalent to the mode, so the kernel stores it instead of
    /// folding it into the mode bits.
    #[cfg(target_os = "linux")]
    fn extended_acl_blob() -> Vec<u8> {
        let mut blob = 2u32.to_le_bytes().to_vec();
        for (tag, perm, id) in [
            (0x0001u16, 0o7u16, u32::MAX), // ACL_USER_OBJ
            (0x0002, 0o5, 0),              // ACL_USER, uid 0
            (0x0004, 0o5, u32::MAX),       // ACL_GROUP_OBJ
            (0x0010, 0o5, u32::MAX),       // ACL_MASK
            (0x0020, 0o0, u32::MAX),       // ACL_OTHER
        ] {
            blob.extend_from_slice(&tag.to_le_bytes());
            blob.extend_from_slice(&perm.to_le_bytes());
            blob.extend_from_slice(&id.to_le_bytes());
        }
        blob
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn posix_acl_survives_the_swap() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wavecode.exe");
        std::fs::write(&target, b"old-binary-bytes").unwrap();
        let acl = extended_acl_blob();
        xattr::set(&target, "system.posix_acl_access", &acl).unwrap();
        // Setting the ACL syncs the mode to its canonical form
        // (owner rwx, mask r-x, other none = 0o750).
        apply_swap(&target, b"new-binary-bytes").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new-binary-bytes");
        let carried = xattr::get(&target, "system.posix_acl_access")
            .unwrap()
            .expect("the access ACL follows the replacement");
        assert_eq!(carried, acl, "the copied ACL matches the original");
        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o750, "the mode stays mirrored");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn no_acl_keeps_the_swap_on_the_mode_mirror() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wavecode.exe");
        std::fs::write(&target, b"old-binary-bytes").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o750)).unwrap();
        apply_swap(&target, b"new-binary-bytes").unwrap();
        assert_eq!(
            xattr::get(&target, "system.posix_acl_access").unwrap(),
            None,
            "no ACL must be invented"
        );
        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o750);
    }

    /// Serialize the path's extended ACL to bytes (`acl_copy_ext`), so
    /// the tests compare before/after blobs instead of asserting bare
    /// presence. `None` when the path carries no ACL.
    #[cfg(all(test, target_os = "macos"))]
    fn macos_acl_blob(path: &std::path::Path) -> Option<Vec<u8>> {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt as _;

        let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: `c_path` is a valid C string; the handle is released
        // on every path below.
        // nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
        let acl = unsafe {
            super::acl_ffi::acl_get_file(c_path.as_ptr(), super::acl_ffi::ACL_TYPE_EXTENDED)
        };
        if acl.is_null() {
            return None;
        }
        // SAFETY: `acl` is a valid handle from the call above.
        // nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
        let size = unsafe { super::acl_ffi::acl_size(acl) };
        assert!(
            size > 0,
            "acl_size failed: {}",
            std::io::Error::last_os_error()
        );
        let mut blob = vec![0u8; size as usize];
        // SAFETY: `blob` has the capacity `acl_size` reported.
        // nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
        let written = unsafe { super::acl_ffi::acl_copy_ext(blob.as_mut_ptr().cast(), acl, size) };
        // SAFETY: `acl` is owned and still valid.
        // nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
        let _released = unsafe { super::acl_ffi::acl_free(acl) };
        assert!(
            written > 0,
            "acl_copy_ext failed: {}",
            std::io::Error::last_os_error()
        );
        blob.truncate(written as usize);
        Some(blob)
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_acl_survives_the_swap() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wavecode.exe");
        std::fs::write(&target, b"old-binary-bytes").unwrap();
        let granted = std::process::Command::new("/bin/chmod")
            .args(["+a", "admin allow read,execute"])
            .arg(&target)
            .output()
            .expect("chmod runs");
        assert!(granted.status.success(), "chmod +a failed: {:?}", granted);
        let original = macos_acl_blob(&target).expect("the target carries an ACL");
        apply_swap(&target, b"new-binary-bytes").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new-binary-bytes");
        let carried = macos_acl_blob(&target).expect("the ACL follows the replacement");
        assert_eq!(carried, original, "the copied ACL matches the original");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn no_macos_acl_keeps_the_swap_on_the_mode_mirror() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wavecode.exe");
        std::fs::write(&target, b"old-binary-bytes").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o750)).unwrap();
        apply_swap(&target, b"new-binary-bytes").unwrap();
        assert_eq!(macos_acl_blob(&target), None, "no ACL must be invented");
        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o750);
    }

    /// A failure anywhere in the staging section must not leave the
    /// partial download behind.
    #[test]
    fn a_failed_stage_removes_the_staging_file() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staged-binary");
        std::fs::write(&staging, b"partial-download").unwrap();
        let error = guarded_staging(&staging, || Err(anyhow::anyhow!("stage failed")))
            .expect_err("the body's error must surface");
        assert!(error.to_string().contains("stage failed"), "{error}");
        assert!(!staging.exists(), "the staging file was cleaned up");
        // A successful stage keeps the file for the rename ahead.
        std::fs::write(&staging, b"partial-download").unwrap();
        guarded_staging(&staging, || Ok(())).unwrap();
        assert!(staging.exists(), "the staged file survives a pass");
    }

    /// A default ACL on the target's directory hands the freshly
    /// created staging file an access ACL the installed binary never
    /// carried; the swap must strip it instead of installing those
    /// grants on the replacement.
    #[cfg(target_os = "linux")]
    #[test]
    fn inherited_default_acl_is_stripped_from_the_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wavecode.exe");
        std::fs::write(&target, b"old-binary-bytes").unwrap();
        let expected_mode = std::fs::metadata(&target).unwrap().permissions().mode();
        xattr::set(dir.path(), "system.posix_acl_default", &extended_acl_blob()).unwrap();
        apply_swap(&target, b"new-binary-bytes").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new-binary-bytes");
        assert_eq!(
            xattr::get(&target, "system.posix_acl_access").unwrap(),
            None,
            "the inherited ACL must be stripped, not installed"
        );
        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode, expected_mode, "the mode stays mirrored");
    }

    /// Only the kernel's "no ACL xattrs on this filesystem" answer
    /// reads as absence; errors that could conceal a grant keep
    /// failing the swap.
    #[cfg(target_os = "linux")]
    #[test]
    fn only_acl_xattr_gaps_read_as_unsupported() {
        // Linux errno 95 (EOPNOTSUPP) is the kernel's "this filesystem
        // keeps no such xattr namespace" answer.
        let unsupported = std::io::Error::from_raw_os_error(95);
        assert!(
            acl_xattrs_unsupported(&unsupported),
            "EOPNOTSUPP ({unsupported}) reads as unsupported"
        );
        assert!(acl_xattrs_unsupported(&std::io::Error::from(
            std::io::ErrorKind::Unsupported
        )));
        for concealing in [
            std::io::Error::from_raw_os_error(libc::EACCES),
            std::io::Error::from_raw_os_error(libc::EIO),
            std::io::Error::from_raw_os_error(libc::EPERM),
        ] {
            assert!(
                !acl_xattrs_unsupported(&concealing),
                "{concealing} must not read as unsupported"
            );
        }
    }

    /// `file_inherit` entries on the target's directory pass the entry
    /// to every file created inside — including the swap's staging
    /// file; the swap must strip it instead of installing it.
    #[cfg(target_os = "macos")]
    #[test]
    fn inherited_macos_acl_is_stripped_from_the_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wavecode.exe");
        std::fs::write(&target, b"old-binary-bytes").unwrap();
        let granted = std::process::Command::new("/bin/chmod")
            .args(["+a", "everyone allow read,file_inherit"])
            .arg(dir.path())
            .output()
            .expect("chmod runs");
        assert!(granted.status.success(), "chmod +a failed: {:?}", granted);
        apply_swap(&target, b"new-binary-bytes").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new-binary-bytes");
        assert_eq!(
            macos_acl_blob(&target),
            None,
            "the inherited ACL must be stripped, not installed"
        );
    }
}
