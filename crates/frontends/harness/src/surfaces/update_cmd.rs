//! `wavecode update`: release check and self-install runners.
//!
//! Both runners wrap `crate::update` primitives; a failed probe exits
//! 1 so scripts can tell "no update" from "could not tell".

use crate::Outcome;
use crate::update;

/// Probe the newest published release and report the comparison.
///
/// `--debug` aside, this surface has no session; a network failure
/// prints and exits 1 so callers never read a failed probe as "no
/// update available".
pub(crate) async fn run_update_check() {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build();
    let client = match client {
        Ok(client) => client,
        Err(cause) => {
            eprintln!("[fail] update check failed: {cause}");
            std::process::exit(Outcome::Failed.exit_code())
        }
    };
    match update::fetch_latest(&client).await {
        Ok(None) => println!("no published release yet"),
        Ok(Some((tag, url))) => match update::classify(&tag, &url) {
            update::UpdateStatus::UpToDate { latest } => {
                println!(
                    "wavecode {} is up to date (latest {latest})",
                    env!("CARGO_PKG_VERSION")
                );
            }
            update::UpdateStatus::Available { latest, url } => {
                println!(
                    "update available: {} -> {latest}",
                    env!("CARGO_PKG_VERSION")
                );
                println!("{url}");
            }
        },
        Err(cause) => {
            eprintln!("[fail] update check failed: {cause:#}");
            std::process::exit(Outcome::Failed.exit_code())
        }
    }
}

/// Verify the downloaded bytes against the published checksum, then
/// swap them in as the running binary. Verification failure returns
/// before [`update::apply_swap`] runs, so mismatched bytes never touch
/// the file on disk.
fn verify_and_install(
    exe: &std::path::Path,
    bytes: &[u8],
    checksum: &str,
) -> anyhow::Result<()> {
    update::verify_sha256(bytes, checksum)?;
    update::apply_swap(exe, bytes)
}

/// Download the newest release and replace this binary with it.
///
/// Refuses source builds (nothing installed to replace) and
/// unpublished targets; verifies the download against the published
/// sha256 before touching the running file, and keeps the replaced
/// binary as `.bak` next to it for manual rollback.
pub(crate) async fn run_update_install() {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build();
    let client = match client {
        Ok(client) => client,
        Err(cause) => {
            eprintln!("[fail] update failed: {cause}");
            std::process::exit(Outcome::Failed.exit_code())
        }
    };
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(cause) => {
            eprintln!("[fail] cannot locate the running binary: {cause}");
            std::process::exit(Outcome::Failed.exit_code())
        }
    };
    if update::running_from_source(&exe) {
        eprintln!(
            "[fail] this is a source build (target/); self-update replaces
                      installed binaries only — use cargo build --release instead"
        );
        std::process::exit(Outcome::Failed.exit_code())
    }
    let Some(target) = update::current_target() else {
        eprintln!(
            "[fail] no prebuilt release for this platform ({}, {}); build
                      from source instead",
            std::env::consts::OS,
            std::env::consts::ARCH
        );
        std::process::exit(Outcome::Failed.exit_code())
    };

    let release = match update::fetch_release(&client).await {
        Ok(Some(release)) => release,
        Ok(None) => {
            eprintln!("[fail] no published release yet");
            std::process::exit(Outcome::Failed.exit_code())
        }
        Err(cause) => {
            eprintln!("[fail] update failed: {cause:#}");
            std::process::exit(Outcome::Failed.exit_code())
        }
    };
    let Some((bin, sha)) = update::pick_assets(&release.tag, &release.assets, target) else {
        eprintln!(
            "[fail] release {tag} carries no binary for {target}",
            tag = release.tag
        );
        std::process::exit(Outcome::Failed.exit_code())
    };

    println!("downloading {} ...", release.tag);
    let bytes = match client.get(&bin.url).send().await {
        Ok(response) => match response.bytes().await {
            Ok(bytes) => bytes,
            Err(cause) => {
                eprintln!("[fail] download failed: {cause}");
                std::process::exit(Outcome::Failed.exit_code())
            }
        },
        Err(cause) => {
            eprintln!("[fail] download failed: {cause}");
            std::process::exit(Outcome::Failed.exit_code())
        }
    };
    let checksum = match client.get(&sha.url).send().await {
        Ok(response) => match response.text().await {
            Ok(text) => text,
            Err(cause) => {
                eprintln!("[fail] checksum download failed: {cause}");
                std::process::exit(Outcome::Failed.exit_code())
            }
        },
        Err(cause) => {
            eprintln!("[fail] checksum download failed: {cause}");
            std::process::exit(Outcome::Failed.exit_code())
        }
    };
    if let Err(cause) = verify_and_install(&exe, &bytes, &checksum) {
        eprintln!("[fail] {cause:#}");
        std::process::exit(Outcome::Failed.exit_code())
    }
    println!("installed {} -> {}", release.tag, exe.display());
    #[cfg(windows)]
    println!(
        "the previous binary was kept as {}",
        exe.with_file_name(format!(
            "{}.bak",
            exe.file_name().unwrap_or_default().to_string_lossy()
        ))
        .display()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// sha256("abc") — the canonical test vector.
    const ABC_CHECKSUM: &str =
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  wavecode-bin\n";

    /// The safety ordering of the install tail: a checksum mismatch
    /// refuses the install before the swap runs, so the binary on disk
    /// keeps its old bytes and no rollback copy or staging file
    /// appears.
    #[test]
    fn checksum_mismatch_refuses_to_touch_the_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wavecode.exe");
        std::fs::write(&target, b"old-binary-bytes").unwrap();

        let error = verify_and_install(&target, b"tampered-bytes", ABC_CHECKSUM)
            .expect_err("mismatched bytes must not install");
        assert!(error.to_string().contains("checksum mismatch"), "{error}");

        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"old-binary-bytes",
            "the running binary must be untouched"
        );
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                name.ends_with(".bak") || name.contains(".download-")
            })
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    /// The matching counterpart: verified bytes do install, so the
    /// refusal above proves an ordering, not a dead no-op.
    #[test]
    fn verified_bytes_swap_the_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("wavecode.exe");
        std::fs::write(&target, b"old-binary-bytes").unwrap();

        verify_and_install(&target, b"abc", ABC_CHECKSUM).expect("matching bytes install");
        assert_eq!(std::fs::read(&target).unwrap(), b"abc");
    }
}
