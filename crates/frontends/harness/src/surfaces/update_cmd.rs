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
    if let Err(cause) = update::verify_sha256(&bytes, &checksum) {
        eprintln!("[fail] {cause:#}");
        std::process::exit(Outcome::Failed.exit_code())
    }

    match update::apply_swap(&exe, &bytes) {
        Ok(()) => {
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
        Err(cause) => {
            eprintln!("[fail] install failed: {cause:#}");
            std::process::exit(Outcome::Failed.exit_code())
        }
    }
}
