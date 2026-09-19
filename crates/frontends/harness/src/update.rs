//! Update check: compare the running version against the newest
//! published GitHub release on the binary-release repo.
//!
//! Explicit surface only (`wavecode update`): no startup probing, no
//! background download. A check failure is reported and exits non-zero
//! instead of ever degrading into a false "up to date".

use anyhow::{Context as _, Result};
use serde_json::Value;

/// Repository whose GitHub Releases carry `wavecode` binaries.
const RELEASES_REPO: &str = "daftpunkwav/wave-code";

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
    let page = payload["html_url"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    Ok(Some((tag, page)))
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
}
