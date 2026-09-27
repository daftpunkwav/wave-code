/*!
 * @file WorkspaceLayers
 * @description Mechanical enforcement of the workspace layer rules:
 * read the real graph via `cargo metadata`, then fail on any normal
 * dependency pointing strictly upward against the tier order below,
 * any frontend reaching outside its allowlist, or anything outside
 * the frontends depending on the composition root. The tier table
 * must stay exhaustive, so a new crate fails until it is placed
 * deliberately.
 */

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

/// Tiers bottom-up: a dependency may only point from a higher tier to
/// a lower one.
const TIERS: &[&str] = &[
    "infrastructure",
    "vocabulary (shared DTOs and passive mechanisms)",
    "foundation",
    "capabilities",
    "state",
    "safety",
    "action",
    "runtime",
    "operations",
    "frontends",
];

/// Tier membership as `"<tier index>: <crate> <crate> ..."` lines.
///
/// `runtime-child`, `transport-mcp`, `action-tasks`, and
/// `runtime-scheduler` sit in the vocabulary tier on purpose: each
/// depends on nothing above infrastructure (action-tasks and
/// runtime-scheduler on nothing internal at all) and is a passive
/// mechanism (child-task bookkeeping, stdio framing, the
/// capability-neutral task seam, durable cron persistence) consumed
/// from several tiers, like the DTO crates.
const TIER_MEMBERS: &[&str] = &[
    "0: infrastructure-base infrastructure-ratelimit",
    "1: action-tasks runtime-child runtime-scheduler transport-mcp wavecode-protocol wavecode-wire",
    "2: wavecode-auth wavecode-config wavecode-llm",
    "3: wavecode-context wavecode-hooks wavecode-mcp wavecode-memory",
    "3: wavecode-sandbox wavecode-skills wavecode-tools",
    "4: state-artifact state-checkpoint state-goal state-persistence state-plan",
    "4: state-store",
    "5: safety-audit safety-gate safety-guardrail safety-secrets",
    "6: action-browser action-jobs action-retrieval action-workflow",
    "7: runtime-plugin runtime-prompt",
    "7: runtime-runner",
    "8: operations-actor operations-bootstrap operations-eval operations-gateway",
    "8: operations-observe operations-simulate",
    "9: console-ui harness-cli tui-engine",
];

/// Internal dependencies per frontend (space-separated), mirroring the
/// post-fix manifests; a new edge needs a deliberate update here,
/// matching the per-crate matrix tests in the frontends.
const FRONTEND_ALLOWLIST: &[(&str, &str)] = &[
    (
        "console-ui",
        "operations-actor state-persistence tui-engine wavecode-config wavecode-wire",
    ),
    (
        "harness-cli",
        "console-ui operations-actor operations-bootstrap operations-eval operations-gateway operations-observe state-persistence wavecode-config wavecode-wire",
    ),
    ("tui-engine", ""),
];

/// Only the frontends may depend on the composition root.
const COMPOSITION_ROOT: &str = "operations-bootstrap";

/// Parse [`TIER_MEMBERS`] into crate -> tier; duplicate listings fail.
fn tier_map() -> BTreeMap<&'static str, usize> {
    let mut map = BTreeMap::new();
    for line in TIER_MEMBERS {
        let (tier, names) = line
            .split_once(": ")
            .expect("tier line is '<tier>: <names>'");
        let tier: usize = tier.parse().expect("tier index parses");
        for name in names.split(' ') {
            assert!(
                map.insert(name, tier).is_none(),
                "crate {name:?} listed twice"
            );
        }
    }
    map
}

#[test]
fn workspace_layers_hold() {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let Ok(output) = Command::new(&cargo)
        .args(["metadata", "--format-version", "1"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
    else {
        eprintln!("skipped: cargo is not available in this environment");
        return;
    };
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let meta: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("cargo metadata emitted valid JSON");

    // Member ids are source URLs (`path+...#name@0.1.0`), so membership
    // is resolved by id and the readable name comes from the package.
    let member_ids: BTreeSet<&str> = meta["workspace_members"]
        .as_array()
        .expect("workspace member ids")
        .iter()
        .map(|id| id.as_str().expect("member id string"))
        .collect();
    let members: BTreeSet<&str> = meta["packages"]
        .as_array()
        .expect("package list")
        .iter()
        .filter(|pkg| member_ids.contains(pkg["id"].as_str().expect("package id string")))
        .map(|pkg| pkg["name"].as_str().expect("package name"))
        .collect();
    let tiers = tier_map();
    assert_eq!(
        members.len(),
        tiers.len(),
        "the tier table and the workspace member set disagree; classify every crate"
    );

    // Normal dependencies between workspace members, from the real graph.
    let mut edges: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for pkg in meta["packages"].as_array().expect("package list") {
        let name = pkg["name"].as_str().expect("package name");
        if !members.contains(name) {
            continue;
        }
        let deps = edges.entry(name).or_default();
        for dep in pkg["dependencies"].as_array().expect("dependency list") {
            if !dep["kind"].is_null() {
                continue; // normal dependencies only: dev/build edges carry no layer weight
            }
            let dep_name = dep["name"].as_str().expect("dependency name");
            if members.contains(dep_name) {
                deps.push(dep_name);
            }
        }
    }

    let tier_of = |name: &str| tiers[name];
    let mut violations = Vec::new();
    for (pkg, deps) in &edges {
        let pkg_tier = tier_of(pkg);
        for dep in deps {
            let dep_tier = tier_of(dep);
            if dep_tier > pkg_tier {
                violations.push(format!(
                    "upward edge {pkg} -> {dep} ({} -> {})",
                    TIERS[pkg_tier], TIERS[dep_tier]
                ));
            }
        }
        if *pkg == COMPOSITION_ROOT {
            continue;
        }
        if deps.contains(&COMPOSITION_ROOT) && !FRONTEND_ALLOWLIST.iter().any(|(f, _)| f == pkg) {
            violations.push(format!(
                "{pkg} depends on {COMPOSITION_ROOT}; only frontends may"
            ));
        }
    }
    for (frontend, allowed) in FRONTEND_ALLOWLIST {
        let actual = edges.get(*frontend).cloned().unwrap_or_default();
        let mut expected: Vec<&str> = allowed.split(' ').filter(|s| !s.is_empty()).collect();
        expected.sort_unstable();
        if actual != expected {
            violations.push(format!(
                "frontend {frontend} internal deps {actual:?} != allowlist {expected:?}"
            ));
        }
    }
    assert!(
        violations.is_empty(),
        "workspace layer violations:\n{}",
        violations.join("\n")
    );
}
