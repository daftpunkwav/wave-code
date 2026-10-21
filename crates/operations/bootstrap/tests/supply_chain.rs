/*!
 * @file WorkspaceSupplyChain
 * @description Mechanical guards for the workspace supply-chain policy.
 *
 * Responsibilities:
 * - Keep the unmaintained yaml-rust parser out of the locked tree.
 * - Keep deny.toml's license allow-list free of unmatched entries.
 *
 * This module must not depend on: any other workspace crate. It reads the
 * workspace's own Cargo.lock, deny.toml, and `cargo metadata` output — the
 * same sources the `cargo deny` gate reads.
 */

use std::path::{Path, PathBuf};
use std::process::Command;

/// The workspace root, three levels above this crate's manifest directory
/// (`crates/operations/bootstrap`).
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("..")
}

/// The locked dependency tree.
fn locked_manifest() -> String {
    let path = workspace_root().join("Cargo.lock");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The supply-chain policy file.
fn deny_policy() -> String {
    let path = workspace_root().join("deny.toml");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The license identifiers listed in `deny.toml`'s `[licenses] allow` array.
///
/// The array holds one quoted identifier per line; comments and the closing
/// bracket end the scan.
fn allowed_licenses(policy: &str) -> Vec<String> {
    let start = policy
        .find("allow = [")
        .expect("deny.toml declares a licenses allow list");
    let body = &policy[start..];
    let end = body.find(']').expect("the licenses allow list is closed");
    body[..end]
        .lines()
        .filter_map(|line| {
            // Drop a trailing comment before reading the quoted identifier.
            let line = line.split('#').next().unwrap_or("");
            let trimmed = line.trim().trim_end_matches(',').trim();
            let quoted = trimmed.strip_prefix('"')?.strip_suffix('"')?;
            Some(quoted.to_owned())
        })
        .collect()
}

/// Whether `license` appears in the SPDX expression `expression` as a whole
/// identifier (so `MIT` never matches inside `LIMITED`).
fn expression_uses(expression: &str, license: &str) -> bool {
    let is_id_char = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '.';
    expression.match_indices(license).any(|(at, _)| {
        let before = expression[..at].chars().next_back();
        let after = expression[at + license.len()..].chars().next();
        !before.is_some_and(is_id_char) && !after.is_some_and(is_id_char)
    })
}

/// The license expressions of every crate in the resolved graph, or `None`
/// when `cargo metadata` cannot run here.
fn graph_licenses() -> Option<Vec<String>> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let Ok(output) = Command::new(&cargo)
        .args(["metadata", "--format-version", "1", "--locked"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
    else {
        eprintln!("skipped: cargo is not available in this environment");
        return None;
    };
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let meta: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("cargo metadata emitted valid JSON");
    let packages = meta["packages"].as_array().expect("package list");
    Some(
        packages
            .iter()
            .filter_map(|pkg| pkg["license"].as_str().map(str::to_owned))
            .collect(),
    )
}

/// The unmaintained yaml-rust parser must stay out of the tree.
///
/// It reached the workspace through syntect's YAML grammar loader
/// (`yaml-load`): syntect parses YAML grammars with yaml-rust, which
/// carries RUSTSEC-2024-0320 ("yaml-rust is unmaintained"). The console
/// never loads a YAML grammar — it reads syntect's bundled binary dumps
/// and one bundled tmTheme — so the loader stays off and yaml-rust stays
/// out. Turning `yaml-load` back on fails here instead of silently
/// re-adding the advisory-flagged crate.
#[test]
fn yaml_rust_stays_out_of_the_locked_tree() {
    let lock = locked_manifest();
    assert!(
        !lock.contains("name = \"yaml-rust\""),
        "yaml-rust (RUSTSEC-2024-0320) is back in the dependency tree"
    );
}

/// Every license the policy allows must be a license the tree actually
/// uses.
///
/// An allowance no crate uses is dead policy: it widens the gate without
/// naming anything real, and it hides the fact that the list was not
/// re-checked against the tree. `cargo deny` reports such entries as
/// "unmatched license allowance" warnings; this test makes the same
/// property a hard failure.
#[test]
fn every_allowed_license_is_used_by_a_crate_in_the_graph() {
    let Some(expressions) = graph_licenses() else {
        return;
    };
    let policy = deny_policy();
    let allowed = allowed_licenses(&policy);
    assert!(
        !allowed.is_empty(),
        "no license allowance parsed out of deny.toml"
    );
    for license in allowed {
        assert!(
            expressions
                .iter()
                .any(|expression| expression_uses(expression, &license)),
            "deny.toml allows {license}, which no crate in the graph uses"
        );
    }
}
