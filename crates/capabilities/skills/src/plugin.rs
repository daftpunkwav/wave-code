/*!
 * @file PluginLoader
 * @description Plugin pack discovery: skill/MCP/hook bundles in the
 * user plugins dir (one `plugin.toml` manifest per pack).
 *
 * Responsibilities:
 * - Discover plugin packs in the user plugins dir.
 * - Validate pack names and load skills through the Discovery pipeline.
 * - Return raw MCP entries and hook rules for the assembly layer.
 * - Warn-and-skip invalid packs without failing assembly.
 *
 * This module must not depend on: workspace crates beyond its own
 * (manifest shapes mirror config/hook types instead of importing them).
 */

//! Plugin packs: skill/MCP/hook bundles installed under
//! `$HOME/.wavecode/plugins/<dir>/plugin.toml`.
//!
//! A manifest carries `{name, version, skills_dir?, hooks_file?}` plus
//! optional `[mcp_servers.<server>]` inline tables. Skills load through
//! the [`discover`] pipeline with [`SkillSource::User`]; MCP entries and
//! hook rules return raw so the composition root (which owns the
//! config/hook dependencies) converts them. Invalid packs warn-and-skip
//! with a reason and never fail assembly.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::{SkillRoot, SkillSet, SkillSource, discover};

/// Maximum plugin name length (the name grammar is `[a-z0-9-]{1,32}`).
const MAX_NAME_LEN: usize = 32;

/// Plugin name grammar: lowercase ASCII letters, digits, and dashes.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Raw MCP server entry from a plugin manifest.
///
/// Mirrors the config-crate server shape without depending on it; the
/// assembly layer converts these into configured servers.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct PluginMcpServer {
    /// Stdio executable command (stdio servers).
    pub command: Option<String>,
    /// Command arguments.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables for the server process.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Streamable-HTTP endpoint (HTTP servers).
    pub url: Option<String>,
    /// Extra request headers.
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

/// Raw hook rule from a plugin hooks file.
///
/// Mirrors the config-crate hook rule without depending on it; the
/// assembly layer converts these into engine definitions.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct PluginHookRule {
    /// Tool-name matcher (optional).
    pub matcher: Option<String>,
    /// Shell command string (required).
    pub command: String,
    /// Timeout in milliseconds (absent means the engine default).
    pub timeout_ms: Option<u64>,
    /// Fire at most once per session (defaults to false).
    pub once: Option<bool>,
}

/// One hooks-file entry: single-table or array-of-tables form, matching
/// the config `[hooks.<EventPoint>]` surface.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(untagged)]
enum HookFileEntry {
    /// Single-table form.
    One(PluginHookRule),
    /// Array-of-tables form (multiple hooks run in file order).
    Many(Vec<PluginHookRule>),
}

/// Raw `plugin.toml` manifest (serde field names are the manifest keys).
#[derive(Debug, Clone, serde::Deserialize)]
struct PluginManifest {
    /// Pack name (`[a-z0-9-]{1,32}`).
    name: String,
    /// Pack version (opaque non-empty string for display).
    version: String,
    /// Skills root relative to the pack dir (each subdir holding a
    /// `SKILL.md` is one skill).
    skills_dir: Option<String>,
    /// Hooks file relative to the pack dir (TOML mapping event points
    /// to hook rules).
    hooks_file: Option<String>,
    /// Inline MCP server tables, keyed by server name.
    #[serde(default)]
    mcp_servers: HashMap<String, PluginMcpServer>,
}

/// One loaded plugin pack: manifest identity plus discovered content.
#[derive(Debug, Default)]
pub struct LoadedPlugin {
    /// Validated pack name.
    pub name: String,
    /// Pack version string.
    pub version: String,
    /// Skills discovered from `skills_dir` ([`SkillSource::User`]).
    pub skills: SkillSet,
    /// Raw MCP entries for assembly, sorted by server name.
    pub mcp_servers: Vec<(String, PluginMcpServer)>,
    /// Raw hook rules for assembly, keyed by event-point name.
    pub hooks: HashMap<String, Vec<PluginHookRule>>,
}

impl LoadedPlugin {
    /// Number of loaded skills (for `plugin list` counts).
    pub fn skill_count(&self) -> usize {
        self.skills.len()
    }

    /// Number of MCP server entries (for `plugin list` counts).
    pub fn mcp_count(&self) -> usize {
        self.mcp_servers.len()
    }

    /// Total hook rules across event points (for `plugin list` counts).
    pub fn hook_count(&self) -> usize {
        self.hooks.values().map(Vec::len).sum()
    }
}

/// Plugin pack loader: discovery plus warn-and-skip validation.
#[derive(Debug, Default)]
pub struct PluginLoader {
    plugins: Vec<LoadedPlugin>,
    warnings: Vec<String>,
}

impl PluginLoader {
    /// Discover packs under `$HOME/.wavecode/plugins`.
    ///
    /// `None` home or a missing plugins root means no packs (a normal
    /// shape, not an error). Every invalid pack warns with its reason
    /// and is skipped; loading never fails.
    pub fn load(home: Option<&Path>) -> Self {
        let mut loader = Self::default();
        let Some(home) = home else {
            return loader;
        };
        let root = home.join(".wavecode").join("plugins");
        let entries = match std::fs::read_dir(&root) {
            Ok(entries) => entries,
            // No plugins root is a normal shape, not an error.
            Err(_) => return loader,
        };
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        dirs.sort();
        for dir in dirs {
            match Self::load_one(&dir) {
                Ok((plugin, mut warnings)) => {
                    loader.warnings.append(&mut warnings);
                    loader.plugins.push(plugin);
                }
                Err(reason) => loader
                    .warnings
                    .push(format!("plugin {} skipped: {reason}", dir.display())),
            }
        }
        loader
    }

    /// Loaded packs in directory order.
    pub fn plugins(&self) -> &[LoadedPlugin] {
        &self.plugins
    }

    /// Load-time warnings in discovery order.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Merge every pack's skills into `set` (same names override, so
    /// later packs win over earlier ones).
    pub fn apply_skills(&self, set: &mut SkillSet) {
        for plugin in &self.plugins {
            for skill in plugin.skills.iter() {
                set.add(skill.clone());
            }
        }
    }

    /// Load one pack dir: manifest validation plus optional parts.
    ///
    /// Manifest-level failures (`Err`) skip the pack; optional-part
    /// failures (a bad hooks file) warn and keep the rest.
    fn load_one(dir: &Path) -> Result<(LoadedPlugin, Vec<String>), String> {
        let path = dir.join("plugin.toml");
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let manifest: PluginManifest =
            toml::from_str(&raw).map_err(|e| format!("invalid {}: {e}", path.display()))?;
        if !valid_name(&manifest.name) {
            return Err(format!(
                "invalid plugin name {:?}: expected [a-z0-9-]{{1,32}}",
                manifest.name
            ));
        }
        if manifest.version.trim().is_empty() {
            return Err("missing version: expected a non-empty string".to_string());
        }
        let mut warnings = Vec::new();
        let skills = match &manifest.skills_dir {
            Some(rel) => {
                // Reuse the Discovery pipeline: same SKILL.md parsing and
                // per-file warn-and-skip, attributed to the User source.
                let roots = [SkillRoot {
                    source: SkillSource::User,
                    dir: dir.join(rel),
                }];
                let discovery = discover(&roots);
                for warning in discovery.warnings {
                    warnings.push(format!("[{}] {warning}", manifest.name));
                }
                discovery.set
            }
            None => SkillSet::default(),
        };
        let mut hooks = HashMap::new();
        if let Some(rel) = &manifest.hooks_file {
            match Self::load_hooks(dir.join(rel)) {
                Ok(rules) => hooks = rules,
                Err(reason) => warnings.push(format!("[{}] {reason}", manifest.name)),
            }
        }
        let mut mcp_servers: Vec<(String, PluginMcpServer)> =
            manifest.mcp_servers.into_iter().collect();
        mcp_servers.sort_by(|a, b| a.0.cmp(&b.0));
        Ok((
            LoadedPlugin {
                name: manifest.name,
                version: manifest.version,
                skills,
                mcp_servers,
                hooks,
            },
            warnings,
        ))
    }

    /// Parse one hooks file into event-point rules.
    fn load_hooks(path: PathBuf) -> Result<HashMap<String, Vec<PluginHookRule>>, String> {
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let table: HashMap<String, HookFileEntry> =
            toml::from_str(&raw).map_err(|e| format!("invalid {}: {e}", path.display()))?;
        Ok(table
            .into_iter()
            .map(|(point, entry)| {
                let rules = match entry {
                    HookFileEntry::One(rule) => vec![rule],
                    HookFileEntry::Many(rules) => rules,
                };
                (point, rules)
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SKILL_MD: &str = "---\ndescription: Review helper skill.\n---\n\nReview body here.\n";

    const MANIFEST: &str = r#"
name = "demo"
version = "0.1.0"
skills_dir = "skills"
hooks_file = "hooks.toml"

[mcp_servers.lint]
command = "lint-server"
args = ["--fast"]
"#;

    const HOOKS: &str = r#"
[PreToolUse]
command = "check.sh"

[[PostToolUse]]
command = "fmt.sh"

[[PostToolUse]]
command = "notify.sh"
"#;

    /// Lay out a home dir holding one pack dir with the given files.
    fn home_with_pack(name: &str, files: &[(&str, &str)]) -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        let pack = home.path().join(".wavecode").join("plugins").join(name);
        std::fs::create_dir_all(&pack).unwrap();
        for (rel, content) in files {
            let path = pack.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(path, content).unwrap();
        }
        home
    }

    #[test]
    fn loads_valid_pack_with_counts() {
        let home = home_with_pack(
            "demo",
            &[
                ("plugin.toml", MANIFEST),
                ("skills/greet/SKILL.md", SKILL_MD),
                ("hooks.toml", HOOKS),
            ],
        );
        let loader = PluginLoader::load(Some(home.path()));
        assert!(loader.warnings().is_empty(), "{:?}", loader.warnings());
        assert_eq!(loader.plugins().len(), 1);
        let plugin = &loader.plugins()[0];
        assert_eq!(plugin.name, "demo");
        assert_eq!(plugin.version, "0.1.0");
        assert_eq!(plugin.skill_count(), 1);
        assert_eq!(plugin.mcp_count(), 1);
        assert_eq!(plugin.hook_count(), 3);
        // MCP entries arrive raw and sorted for assembly.
        assert_eq!(plugin.mcp_servers[0].0, "lint");
        assert_eq!(
            plugin.mcp_servers[0].1.command.as_deref(),
            Some("lint-server")
        );
        assert!(plugin.skills.get("greet").is_some());
    }

    #[test]
    fn invalid_name_skips_pack_with_reason() {
        let home = home_with_pack(
            "bad",
            &[("plugin.toml", "name = \"Bad_Name!\"\nversion = \"1\"\n")],
        );
        let loader = PluginLoader::load(Some(home.path()));
        assert!(loader.plugins().is_empty());
        assert_eq!(loader.warnings().len(), 1);
        assert!(loader.warnings()[0].contains("invalid plugin name"));
    }

    #[test]
    fn overlong_name_skips_pack() {
        let long = "a".repeat(33);
        let home = home_with_pack(
            "long",
            &[(
                "plugin.toml",
                &format!("name = \"{long}\"\nversion = \"1\"\n"),
            )],
        );
        let loader = PluginLoader::load(Some(home.path()));
        assert!(loader.plugins().is_empty());
        assert!(loader.warnings()[0].contains("invalid plugin name"));
    }

    #[test]
    fn bad_toml_skips_pack_with_reason() {
        let home = home_with_pack("broken", &[("plugin.toml", "[[[not toml")]);
        let loader = PluginLoader::load(Some(home.path()));
        assert!(loader.plugins().is_empty());
        assert!(loader.warnings()[0].contains("invalid"));
    }

    #[test]
    fn missing_manifest_skips_dir_with_reason() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".wavecode").join("plugins").join("empty"))
            .unwrap();
        let loader = PluginLoader::load(Some(home.path()));
        assert!(loader.plugins().is_empty());
        assert!(loader.warnings()[0].contains("cannot read"));
    }

    #[test]
    fn bad_hooks_file_warns_but_keeps_skills() {
        let home = home_with_pack(
            "partial",
            &[
                (
                    "plugin.toml",
                    "name = \"partial\"\nversion = \"2\"\nskills_dir = \"skills\"\nhooks_file = \"hooks.toml\"\n",
                ),
                ("skills/greet/SKILL.md", SKILL_MD),
                ("hooks.toml", "[[[not toml"),
            ],
        );
        let loader = PluginLoader::load(Some(home.path()));
        assert_eq!(loader.plugins().len(), 1);
        let plugin = &loader.plugins()[0];
        assert_eq!(plugin.skill_count(), 1);
        assert_eq!(plugin.hook_count(), 0);
        assert_eq!(loader.warnings().len(), 1);
        assert!(loader.warnings()[0].contains("invalid"));
    }

    #[test]
    fn missing_skills_dir_means_zero_skills() {
        let home = home_with_pack(
            "noskills",
            &[("plugin.toml", "name = \"noskills\"\nversion = \"1\"\n")],
        );
        let loader = PluginLoader::load(Some(home.path()));
        assert_eq!(loader.plugins().len(), 1);
        assert_eq!(loader.plugins()[0].skill_count(), 0);
        assert!(loader.warnings().is_empty());
    }

    #[test]
    fn no_home_or_no_root_loads_nothing_quietly() {
        assert!(PluginLoader::load(None).plugins().is_empty());
        assert!(PluginLoader::load(None).warnings().is_empty());
        let home = tempfile::tempdir().unwrap();
        let loader = PluginLoader::load(Some(home.path()));
        assert!(loader.plugins().is_empty());
        assert!(loader.warnings().is_empty());
    }

    #[test]
    fn apply_skills_merges_as_user_source() {
        let home = home_with_pack(
            "demo",
            &[
                ("plugin.toml", MANIFEST),
                ("skills/greet/SKILL.md", SKILL_MD),
            ],
        );
        // No hooks file on disk: warns once, skills still load.
        let loader = PluginLoader::load(Some(home.path()));
        let mut set = SkillSet::default();
        loader.apply_skills(&mut set);
        let skill = set.get("greet").expect("merged skill");
        assert_eq!(skill.source, SkillSource::User);
    }
}
