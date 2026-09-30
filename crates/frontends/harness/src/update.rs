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
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o755))?;
    }
    // Unix rename replaces atomically. Windows refuses to remove a
    // running exe but allows renaming it, so the old binary steps
    // aside as `.bak` first; a failed second step rolls the `.bak`
    // back into place.
    #[cfg(windows)]
    let backup = dir.join(format!("{file_name}.bak"));
    #[cfg(windows)]
    {
        std::fs::rename(target_exe, &backup)
            .with_context(|| format!("moving the old binary to {}", backup.display()))?;
        if let Err(error) = std::fs::rename(&staging, target_exe) {
            let _ = std::fs::rename(&backup, target_exe);
            let _ = std::fs::remove_file(&staging);
            return Err(error).context("install failed; the old binary was restored");
        }
    }
    #[cfg(unix)]
    {
        if let Err(error) = std::fs::rename(&staging, target_exe) {
            let _ = std::fs::remove_file(&staging);
            return Err(error).context("install failed; the old binary was not touched");
        }
    }
    #[cfg(windows)]
    {
        let _ = backup; // kept on purpose as the rollback copy
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let source_release = std::path::Path::new(r"D:\repo\target\release\wavecode.exe");
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
        apply_swap(&target, b"new-binary-bytes").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new-binary-bytes");
        // The old bytes survive as `.bak` next to the target.
        let backup = dir.path().join("wavecode.exe.bak");
        assert_eq!(std::fs::read(&backup).unwrap(), b"old-binary-bytes");
        // No staging leftovers: the download file was renamed away.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".download-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }
}
