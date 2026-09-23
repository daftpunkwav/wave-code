/*!
 * @file PluginInventory
 * @description Plugin pack discovery for frontend listing surfaces.
 *
 * Responsibilities:
 * - Load installed plugin packs and summarize them for display.
 * - Keep pack-format knowledge out of frontends: parsing and validation
 *   live in `wavecode-skills`, this module only maps to display rows.
 *
 * This module must not depend on: drivers, actors, sessions, or models.
 */

//! Plugin pack inventory for the `plugin list` surface.

use std::path::Path;

/// One installed plugin pack, summarized for display.
#[derive(Debug, Clone)]
pub struct PluginSummary {
    /// Validated pack name.
    pub name: String,
    /// Pack version string.
    pub version: String,
    /// Number of skills the pack contributes.
    pub skills: usize,
    /// Number of MCP server entries the pack declares.
    pub mcp_servers: usize,
    /// Number of hook rules the pack declares.
    pub hooks: usize,
}

/// Load every installed plugin pack under `home`.
///
/// Returns the summaries plus non-fatal parse warnings: a broken pack
/// degrades to a warning, never a hard failure.
pub fn discover_plugin_summaries(home: Option<&Path>) -> (Vec<PluginSummary>, Vec<String>) {
    let loader = wavecode_skills::plugin::PluginLoader::load(home);
    let summaries = loader
        .plugins()
        .iter()
        .map(|plugin| PluginSummary {
            name: plugin.name.clone(),
            version: plugin.version.clone(),
            skills: plugin.skill_count(),
            mcp_servers: plugin.mcp_count(),
            hooks: plugin.hook_count(),
        })
        .collect();
    (summaries, loader.warnings().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An empty home lists no packs with no warnings.
    #[test]
    fn empty_home_yields_no_packs() {
        let home = tempfile::tempdir().unwrap();
        let (summaries, warnings) = discover_plugin_summaries(Some(home.path()));
        assert!(summaries.is_empty());
        assert!(warnings.is_empty());
    }

    /// A well-formed pack is summarized with its counts; the summary is
    /// what the frontend prints, so the mapping stays locked.
    #[test]
    fn pack_maps_to_summary_row() {
        let home = tempfile::tempdir().unwrap();
        let pack = home.path().join(".wavecode").join("plugins").join("demo");
        std::fs::create_dir_all(pack.join("skills").join("greet")).unwrap();
        std::fs::write(
            pack.join("plugin.toml"),
            "name = \"demo\"\nversion = \"1.0.0\"\nskills_dir = \"skills\"\n",
        )
        .unwrap();
        std::fs::write(
            pack.join("skills").join("greet").join("SKILL.md"),
            "---\ndescription: Greet helper.\n---\n\nGreet body.\n",
        )
        .unwrap();
        let (summaries, warnings) = discover_plugin_summaries(Some(home.path()));
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(summaries.len(), 1);
        let row = &summaries[0];
        assert_eq!(row.name, "demo");
        assert_eq!(row.version, "1.0.0");
        assert_eq!(row.skills, 1);
        assert_eq!(row.mcp_servers, 0);
        assert_eq!(row.hooks, 0);
    }
}
