//! wavecode-skills — skills system (SPEC section 8, landed in P7).
//!
//! SKILL.md (YAML frontmatter + Markdown body) discovery, parsing, and catalog
//! injection:
//! - Discovery: `<root>/skills/<name>/SKILL.md`, with sources in ascending
//!   priority (same names overridden) builtin < `~/.wavecode/skills` <
//!   `.wavecode/skills` (skills exposed over MCP are the fourth SPEC section
//!   8.1 source, landing with P9 MCP — a placeholder in this version);
//! - Frontmatter fields take the SPEC section 8.1 table intersection:
//!   `description` (required) / `when_to_use` / `allowed-tools` /
//!   `context: inline | fork` / `user-invocable` / `argument-hint` / `paths`;
//! - Catalog injection: [`SkillSet::catalog`] renders the name + description +
//!   when_to_use catalog; the budget (1% of the context window) arrives from
//!   the caller (core) as a character quota, with downgraded truncation past
//!   the limit (drop when_to_use first, then truncate descriptions);
//! - Execution expansion: [`Skill::expand`] substitutes the `$ARGUMENTS`
//!   placeholder and the `${WAVECODE_SKILL_DIR}` variable; the `tool`
//!   module hosts the model-invokable `skill` tool (inline bodies expand
//!   in place, forks spawn through the `action-tasks` seam, the
//!   vocabulary-tier crate below this one).
//!
//! **Frontmatter parsing tradeoff**: `serde_yaml` instead of a hand-rolled
//! minimal parser — frontmatter is YAML (field values may hold colons, lists,
//! multi-line strings), and a hand-rolled parser's edge cases (quotes,
//! indented lists) would silently degrade; serde_yaml is already in the
//! workspace-root `[workspace.dependencies]` at a unified version (SPEC
//! section 3 discipline). The SPEC section 8.1 table mixes kebab-case
//! (`allowed-tools`) with snake_case (`when_to_use`) field names, so the
//! parsing surface accepts both spellings (serde aliases) and constrains
//! neither on the write side.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub mod plugin;
pub mod tool;

/// The `$ARGUMENTS` placeholder (replaced with the call arguments on inline
/// expansion).
const ARGUMENTS_PLACEHOLDER: &str = "$ARGUMENTS";
/// The skill-dir variable (expands to the absolute path of the SKILL.md
/// directory).
const SKILL_DIR_VARIABLE: &str = "${WAVECODE_SKILL_DIR}";

/// Skill source (ascending priority; same-named skills from a higher-priority
/// source override lower-priority ones).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SkillSource {
    /// Builtin skill set shipped with the binary.
    Builtin,
    /// User-level `~/.wavecode/skills`.
    User,
    /// Project-level `<cwd>/.wavecode/skills`.
    Project,
    /// Inline skills converted from prompts an MCP server exposes (SPEC
    /// section 8.1 fourth source / section 10, highest priority). P9 lands
    /// only the enum placeholder; real conversion fetches content via
    /// `prompts/get` and wires up on the core side with the real MCP
    /// transport.
    Mcp,
}

impl SkillSource {
    /// Source name (for diagnostics / warning text).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::User => "user",
            Self::Project => "project",
            Self::Mcp => "mcp",
        }
    }
}

/// Execution mode (the frontmatter `context` field, SPEC section 8.1): inline
/// expands into the current session; fork runs in a dedicated subagent.
/// Defaults to inline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillContext {
    /// Body expands into the current session as a user message.
    #[default]
    Inline,
    /// A dedicated subagent is derived with the skill body as its instructions.
    Fork,
}

/// SKILL.md frontmatter (the SPEC section 8.1 field intersection).
///
/// Field names mix kebab / snake spellings (as in the SPEC table verbatim);
/// both spellings are accepted; unknown fields are ignored (forward
/// compatible — new fields never break old versions).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct SkillMeta {
    /// One-line capability description (required), injected into the catalog.
    pub description: String,
    /// The model's auto-trigger evidence, injected into the catalog.
    #[serde(alias = "when-to-use")]
    pub when_to_use: Option<String>,
    /// Tool-name allowlist while the skill is active (empty = unlimited).
    #[serde(rename = "allowed-tools", alias = "allowed_tools", default)]
    pub allowed_tools: Vec<String>,
    /// Execution mode (inline / fork).
    #[serde(default)]
    pub context: SkillContext,
    /// Whether `/name` direct invocation is allowed (default true).
    #[serde(
        rename = "user-invocable",
        alias = "user_invocable",
        default = "default_true"
    )]
    pub user_invocable: bool,
    /// Parameter hint (for completion).
    #[serde(rename = "argument-hint", alias = "argument_hint")]
    pub argument_hint: Option<String>,
    /// Glob list for conditional activation on file operations (recorded only
    /// in the first version; plays no part in triggering).
    #[serde(default)]
    pub paths: Vec<String>,
}

/// The serde default for `user_invocable` (SPEC: defaults to true).
fn default_true() -> bool {
    true
}

/// One parsed skill: directory name + frontmatter + body.
#[derive(Debug, Clone)]
pub struct Skill {
    /// Skill name (the SKILL.md directory name).
    pub name: String,
    /// The SKILL.md directory (the `${WAVECODE_SKILL_DIR}` expansion target).
    pub dir: PathBuf,
    /// Source (decides override priority).
    pub source: SkillSource,
    /// Frontmatter.
    pub meta: SkillMeta,
    /// Markdown body (everything after the frontmatter, trimmed).
    pub body: String,
}

impl Skill {
    /// Parse one skill directory (`<dir>/SKILL.md`).
    ///
    /// A missing / unreadable SKILL.md or invalid frontmatter (including a
    /// missing `description`) all return Err — the caller ([`discover`]) turns
    /// them into warnings and skips, so one bad file never breaks discovery.
    pub fn parse(dir: &Path, source: SkillSource) -> Result<Self, SkillError> {
        let path = dir.join("SKILL.md");
        let raw = std::fs::read_to_string(&path).map_err(|e| SkillError::Read {
            path: path.clone(),
            reason: e.to_string(),
        })?;
        let (frontmatter, body) = split_frontmatter(&raw).ok_or_else(|| SkillError::Parse {
            path: path.clone(),
            reason: "missing YAML frontmatter (a header block delimited by ---)".to_owned(),
        })?;
        let meta: SkillMeta = serde_yaml::from_str(frontmatter).map_err(|e| SkillError::Parse {
            path: path.clone(),
            reason: e.to_string(),
        })?;
        if meta.description.trim().is_empty() {
            return Err(SkillError::Parse {
                path: path.clone(),
                reason: "description is required and must not be empty".to_owned(),
            });
        }
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| SkillError::Parse {
                path: path.clone(),
                reason: "cannot derive the skill name from the directory path".to_owned(),
            })?;
        Ok(Self {
            name,
            dir: dir.to_path_buf(),
            source,
            meta,
            body: body.trim().to_owned(),
        })
    }

    /// Inline expansion (SPEC section 8.2): `$ARGUMENTS` is replaced with the
    /// call arguments, `${WAVECODE_SKILL_DIR}` with the skill directory path.
    ///
    /// When the body has no `$ARGUMENTS` placeholder but the caller passed
    /// arguments, the arguments are appended at the end of the body (matching
    /// Claude Code behavior: a missing placeholder does not drop arguments).
    pub fn expand(&self, args: &str) -> String {
        let args = args.trim();
        let mut out = self
            .body
            .replace(SKILL_DIR_VARIABLE, &self.dir.display().to_string());
        if out.contains(ARGUMENTS_PLACEHOLDER) {
            out = out.replace(ARGUMENTS_PLACEHOLDER, args);
        } else if !args.is_empty() {
            out.push_str("\n\n");
            out.push_str(args);
        }
        out
    }
}

/// Split frontmatter from body: the file starts with a `---` line and the next
/// line holding only `---` closes it; the YAML frontmatter sits between, the
/// rest is the body. None means no valid frontmatter.
///
/// A closing line allows only `---` plus end-of-line whitespace; `---`
/// followed by same-line content (e.g. `--- junk`) is not a legal close (the
/// search continues to the next candidate, else None) — otherwise a malformed
/// close would silently pollute the body (its same-line remainder leaking into
/// it).
fn split_frontmatter(raw: &str) -> Option<(&str, &str)> {
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let mut lines = raw.splitn(2, '\n');
    if lines.next()?.trim_end() != "---" {
        return None;
    }
    let rest = lines.next()?;
    let mut base = 0;
    let mut search = rest;
    loop {
        let idx = search.find("\n---")?;
        let end = base + idx;
        let after = &rest[end + 4..];
        let fence_rest = after.split('\n').next().unwrap_or("");
        if fence_rest.trim().is_empty() {
            let frontmatter = &rest[..end];
            let tail = &after[fence_rest.len()..];
            let body = tail
                .strip_prefix('\n')
                .or_else(|| tail.strip_prefix("\r\n"))
                .unwrap_or(tail);
            return Some((frontmatter, body));
        }
        // Not a standalone line: not a legal close; keep searching for the
        // next candidate after this newline.
        base = end + 1;
        search = &rest[base..];
    }
}

/// Skill parse / read errors (turned into warnings at discovery).
#[derive(Debug, thiserror::Error)]
pub enum SkillError {
    /// SKILL.md read failure.
    #[error("failed to read {}: {reason}", .path.display())]
    Read {
        /// The offending file.
        path: PathBuf,
        /// The underlying reason.
        reason: String,
    },
    /// Frontmatter parse failure (including missing required fields).
    #[error("failed to parse {}: {reason}", .path.display())]
    Parse {
        /// The offending file.
        path: PathBuf,
        /// The underlying reason.
        reason: String,
    },
}

/// One discovery root: a source plus a directory (`<dir>/<name>/SKILL.md`).
#[derive(Debug, Clone)]
pub struct SkillRoot {
    /// Source (priority).
    pub source: SkillSource,
    /// Skills root directory (each subdirectory holding a SKILL.md is one
    /// skill).
    pub dir: PathBuf,
}

/// Standard discovery roots (SPEC section 8.1 priority, low to high): builtin
/// (if any) < `~/.wavecode/skills` < `<cwd>/.wavecode/skills`.
pub fn standard_roots(builtin: Option<PathBuf>, home: Option<&Path>, cwd: &Path) -> Vec<SkillRoot> {
    let mut roots = Vec::new();
    if let Some(dir) = builtin {
        roots.push(SkillRoot {
            source: SkillSource::Builtin,
            dir,
        });
    }
    if let Some(home) = home {
        roots.push(SkillRoot {
            source: SkillSource::User,
            dir: home.join(".wavecode").join("skills"),
        });
    }
    roots.push(SkillRoot {
        source: SkillSource::Project,
        dir: cwd.join(".wavecode").join("skills"),
    });
    roots
}

/// Discovery product: the skill set plus warnings (each bad file warns and
/// skips without breaking overall discovery).
///
/// The roots are retained so [`Discovery::refresh`] can re-sweep them later
/// (poll-based watching: callers re-sweep on an interval; automatic periodic
/// refresh with background threads is a documented future, not this version —
/// a library crate must not spawn hidden threads).
#[derive(Debug, Default)]
pub struct Discovery {
    /// The override-resolved skill set.
    pub set: SkillSet,
    /// Discovery-time warnings (read / parse failures).
    pub warnings: Vec<String>,
    roots: Vec<SkillRoot>,
}

impl Discovery {
    /// Re-sweep the original roots and swap in the new skill set (new files
    /// picked up, deleted files dropped, edits re-parsed; same-name override
    /// order preserved). New warnings append to [`Discovery::warnings`].
    pub fn refresh(&mut self) {
        let roots = std::mem::take(&mut self.roots);
        let fresh = discover(&roots);
        self.roots = roots;
        self.set.replace(fresh.set);
        self.warnings.extend(fresh.warnings);
    }
}

/// Discover every skill in priority order: `roots` must arrive low-priority
/// first, and same-named skills from later (higher-priority) roots override
/// earlier ones (SPEC section 8.1). Missing / unreadable root dirs are skipped
/// silently (a missing source is a normal shape); individual bad skill files
/// warn and continue.
pub fn discover(roots: &[SkillRoot]) -> Discovery {
    let mut discovery = Discovery {
        roots: roots.to_vec(),
        ..Default::default()
    };
    for root in roots {
        let entries = match std::fs::read_dir(&root.dir) {
            Ok(entries) => entries,
            // Missing / unreadable root dir: the source is absent, not an error.
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() || !dir.join("SKILL.md").is_file() {
                continue;
            }
            match Skill::parse(&dir, root.source) {
                Ok(skill) => {
                    // Same-name override: the higher-priority source (handled
                    // later) replaces the lower-priority one.
                    discovery.set.skills.insert(skill.name.clone(), skill);
                }
                Err(e) => {
                    discovery
                        .warnings
                        .push(format!("[{}] skill skipped: {e}", root.source.as_str()));
                }
            }
        }
    }
    discovery
}

/// The override-resolved skill set (ordered by name, so iteration output is
/// stable).
#[derive(Debug, Default)]
pub struct SkillSet {
    skills: BTreeMap<String, Skill>,
}

impl SkillSet {
    /// Insert one skill directly (same names override). The injection point
    /// outside the discovery pipeline: unit-test construction, and merging
    /// MCP-exposed skills later (P9).
    pub fn add(&mut self, skill: Skill) {
        self.skills.insert(skill.name.clone(), skill);
    }

    /// Swap the whole set for `other` (used by [`Discovery::refresh`] to
    /// install a re-swept set without disturbing handles on `self`).
    pub fn replace(&mut self, other: SkillSet) {
        self.skills = other.skills;
    }

    /// Look up by name.
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.get(name)
    }

    /// Iterate (in name dictionary order).
    pub fn iter(&self) -> impl Iterator<Item = &Skill> {
        self.skills.values()
    }

    /// Skill count.
    pub fn len(&self) -> usize {
        self.skills.len()
    }

    /// Whether empty.
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// Render the catalog injection text (SPEC section 8.2: name +
    /// description + when_to_use; `max_chars` is a character quota — the
    /// budget = 1% of the context window, converted by core).
    ///
    /// Downgrade policy past the limit (stepwise): full (with when_to_use) ->
    /// without when_to_use -> descriptions truncated to a per-entry quota
    /// (ending in `…`) -> a hard cut guarding the total. A zero quota or no
    /// skills returns an empty string (the caller drops the injection slot).
    pub fn catalog(&self, max_chars: usize) -> String {
        if self.skills.is_empty() || max_chars == 0 {
            return String::new();
        }
        let full = self.render(true, None);
        // Budgets compare by character count (matching max_chars' "character
        // quota" contract): String::len is bytes, and CJK descriptions take 3
        // bytes per char — comparing by bytes would trigger downgrade
        // truncation two to three times too early.
        if full.chars().count() <= max_chars {
            return full;
        }
        let no_when = self.render(false, None);
        if no_when.chars().count() <= max_chars {
            return no_when;
        }
        // Per-entry quota: each entry costs ~8 chars of "- : \n" overhead;
        // floor at 16 against over-truncation.
        let per_entry = (max_chars / self.skills.len()).saturating_sub(8).max(16);
        let truncated = self.render(false, Some(per_entry));
        if truncated.chars().count() <= max_chars {
            return truncated;
        }
        // Final hard-cut fallback: clip by character count and guarantee the
        // result fits `max_chars` (byte indices would chop CJK text several
        // times too short; the fixed suffix itself runs ~28 chars, so tiny
        // quotas cut with no suffix — otherwise the suffix alone would exceed
        // the budget).
        const TRUNC_SUFFIX: &str = "\n…(skills catalog truncated)";
        let suffix_len = TRUNC_SUFFIX.chars().count();
        if max_chars <= suffix_len {
            return truncated.chars().take(max_chars).collect();
        }
        let kept: String = truncated.chars().take(max_chars - suffix_len).collect();
        format!("{kept}{TRUNC_SUFFIX}")
    }

    /// Catalog rendering: `include_when` toggles the when_to_use suffix;
    /// `desc_limit` is the per-description truncation quota (None keeps full
    /// text).
    fn render(&self, include_when: bool, desc_limit: Option<usize>) -> String {
        let mut out = String::new();
        for skill in self.skills.values() {
            let desc = match desc_limit {
                Some(limit) => truncate_chars(skill.meta.description.trim(), limit),
                None => skill.meta.description.trim().to_owned(),
            };
            out.push_str("- ");
            out.push_str(&skill.name);
            out.push_str(": ");
            out.push_str(&desc);
            if include_when && let Some(when) = &skill.meta.when_to_use {
                let when = when.trim();
                if !when.is_empty() {
                    out.push_str(" (when: ");
                    out.push_str(when);
                    out.push(')');
                }
            }
            out.push('\n');
        }
        out.trim_end().to_owned()
    }
}

/// Truncate by character count (cut tail + `…` past the limit; UTF-8 boundary
/// safe).
fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let mut t: String = text.chars().take(limit.saturating_sub(1)).collect();
    t.push('…');
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &Path, name: &str, frontmatter: &str, body: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), format!("{frontmatter}\n{body}")).unwrap();
    }

    // —— frontmatter parsing ——

    #[test]
    fn parses_full_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(
            dir.path(),
            "commit",
            r#"---
description: Create conventional git commits
when_to_use: When the user asks to commit code
allowed-tools:
  - shell
  - read_file
context: fork
user-invocable: false
argument-hint: "[message]"
paths:
  - "src/**"
---"#,
            "Body: commit by the book.",
        );
        let root = SkillRoot {
            source: SkillSource::Project,
            dir: dir.path().to_path_buf(),
        };
        let discovery = discover(&[root]);
        assert!(discovery.warnings.is_empty(), "{:?}", discovery.warnings);
        let skill = discovery.set.get("commit").unwrap();
        assert_eq!(skill.meta.description, "Create conventional git commits");
        assert_eq!(
            skill.meta.when_to_use.as_deref(),
            Some("When the user asks to commit code")
        );
        assert_eq!(skill.meta.allowed_tools, vec!["shell", "read_file"]);
        assert_eq!(skill.meta.context, SkillContext::Fork);
        assert!(!skill.meta.user_invocable);
        assert_eq!(skill.meta.argument_hint.as_deref(), Some("[message]"));
        assert_eq!(skill.meta.paths, vec!["src/**"]);
        assert_eq!(skill.body, "Body: commit by the book.");
        assert_eq!(skill.source, SkillSource::Project);
    }

    #[test]
    fn defaults_and_alias_spellings() {
        let dir = tempfile::tempdir().unwrap();
        // snake_case spellings (the SPEC table mixes kebab/snake; both are
        // accepted) + defaults.
        write_skill(
            dir.path(),
            "review",
            "---\ndescription: Review code\nwhen-to-use: When review is mentioned\nallowed_tools: [grep]\n---",
            "Review body",
        );
        let root = SkillRoot {
            source: SkillSource::User,
            dir: dir.path().to_path_buf(),
        };
        let discovery = discover(&[root]);
        assert!(discovery.warnings.is_empty(), "{:?}", discovery.warnings);
        let skill = discovery.set.get("review").unwrap();
        assert_eq!(
            skill.meta.when_to_use.as_deref(),
            Some("When review is mentioned")
        );
        assert_eq!(skill.meta.allowed_tools, vec!["grep"]);
        // Defaults: inline / user_invocable=true / no hint / no paths.
        assert_eq!(skill.meta.context, SkillContext::Inline);
        assert!(skill.meta.user_invocable);
        assert!(skill.meta.argument_hint.is_none());
        assert!(skill.meta.paths.is_empty());
    }

    #[test]
    fn missing_or_empty_description_is_warning_skip() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "nodesc", "---\nwhen_to_use: x\n---", "Body");
        write_skill(
            dir.path(),
            "emptydesc",
            "---\ndescription: \"\"\n---",
            "Body",
        );
        let root = SkillRoot {
            source: SkillSource::User,
            dir: dir.path().to_path_buf(),
        };
        let discovery = discover(&[root]);
        assert_eq!(discovery.set.len(), 0);
        assert_eq!(discovery.warnings.len(), 2);
        assert!(
            discovery.warnings[0].contains("description")
                || discovery.warnings[1].contains("description")
        );
    }

    #[test]
    fn file_without_frontmatter_is_warning_skip() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "plain", "No header", "Body");
        let root = SkillRoot {
            source: SkillSource::User,
            dir: dir.path().to_path_buf(),
        };
        let discovery = discover(&[root]);
        assert_eq!(discovery.set.len(), 0);
        assert_eq!(discovery.warnings.len(), 1);
    }

    // —— source priority ——

    /// SPEC section 8 acceptance: same-name override, builtin < user < project.
    #[test]
    fn higher_priority_source_overrides_same_name() {
        let builtin = tempfile::tempdir().unwrap();
        let user = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        for (root, desc) in [
            (builtin.path(), "builtin edition"),
            (user.path(), "user edition"),
            (project.path(), "project edition"),
        ] {
            write_skill(
                root,
                "lint",
                &format!("---\ndescription: {desc}\n---"),
                "Body",
            );
        }
        // Skills living only in a lower-priority source survive.
        write_skill(
            user.path(),
            "only-user",
            "---\ndescription: user-level only\n---",
            "Body",
        );
        let roots = [
            SkillRoot {
                source: SkillSource::Builtin,
                dir: builtin.path().to_path_buf(),
            },
            SkillRoot {
                source: SkillSource::User,
                dir: user.path().to_path_buf(),
            },
            SkillRoot {
                source: SkillSource::Project,
                dir: project.path().to_path_buf(),
            },
        ];
        let discovery = discover(&roots);
        let lint = discovery.set.get("lint").unwrap();
        assert_eq!(lint.meta.description, "project edition");
        assert_eq!(lint.source, SkillSource::Project);
        let only_user = discovery.set.get("only-user").unwrap();
        assert_eq!(only_user.source, SkillSource::User);
        // Missing root dirs skip silently.
        let missing = discover(&[SkillRoot {
            source: SkillSource::User,
            dir: user.path().join("nope"),
        }]);
        assert!(missing.set.is_empty() && missing.warnings.is_empty());
    }

    // —— inline expansion ——

    /// SPEC section 8 acceptance: $ARGUMENTS substitution and the
    /// ${WAVECODE_SKILL_DIR} variable.
    #[test]
    fn expand_replaces_arguments_and_skill_dir() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(
            dir.path(),
            "fix",
            "---\ndescription: Fix issues\n---",
            "Fix $ARGUMENTS, see ${WAVECODE_SKILL_DIR}/notes.md",
        );
        let root = SkillRoot {
            source: SkillSource::Project,
            dir: dir.path().to_path_buf(),
        };
        let discovery = discover(&[root]);
        let skill = discovery.set.get("fix").unwrap();
        let expanded = skill.expand("the crash");
        assert_eq!(
            expanded,
            format!("Fix the crash, see {}/notes.md", skill.dir.display())
        );
        // No arguments: the placeholder expands to an empty string.
        assert!(skill.expand("").contains("Fix , see"));
    }

    #[test]
    fn expand_appends_args_when_placeholder_missing() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(
            dir.path(),
            "plain",
            "---\ndescription: No placeholder\n---",
            "Run by the book.",
        );
        let root = SkillRoot {
            source: SkillSource::Project,
            dir: dir.path().to_path_buf(),
        };
        let discovery = discover(&[root]);
        let skill = discovery.set.get("plain").unwrap();
        assert_eq!(skill.expand("extra args"), "Run by the book.\n\nextra args");
        assert_eq!(skill.expand(""), "Run by the book.");
    }

    // —— catalog injection ——

    fn catalog_set(entries: &[(&str, &str, Option<&str>)]) -> SkillSet {
        let mut skills = BTreeMap::new();
        for (name, desc, when) in entries {
            skills.insert(
                name.to_string(),
                Skill {
                    name: name.to_string(),
                    dir: PathBuf::from("/tmp/x"),
                    source: SkillSource::Project,
                    meta: SkillMeta {
                        description: desc.to_string(),
                        when_to_use: when.map(str::to_owned),
                        allowed_tools: vec![],
                        context: SkillContext::Inline,
                        user_invocable: true,
                        argument_hint: None,
                        paths: vec![],
                    },
                    body: String::new(),
                },
            );
        }
        SkillSet { skills }
    }

    #[test]
    fn catalog_renders_name_description_when() {
        let set = catalog_set(&[
            (
                "commit",
                "Create commits",
                Some("when the user asks to commit"),
            ),
            ("review", "Review code", None),
        ]);
        let catalog = set.catalog(10_000);
        assert!(catalog.contains("- commit: Create commits (when: when the user asks to commit)"));
        assert!(catalog.contains("- review: Review code"));
        // Empty set / zero quota -> empty string (slot dropped).
        assert!(SkillSet::default().catalog(10_000).is_empty());
        assert!(set.catalog(0).is_empty());
    }

    /// SPEC section 8 acceptance: over-budget truncation — when_to_use goes
    /// first, then descriptions; the result never exceeds any quota.
    #[test]
    fn catalog_truncates_to_budget() {
        let long_desc = "A very long capability description that blows the injection budget.";
        let long_when = "This when_to_use is likewise long and must yield to the budget.";
        let entries: Vec<(String, String, Option<String>)> = (0..20)
            .map(|i| {
                (
                    format!("skill-{i:02}"),
                    format!("{long_desc}{i}"),
                    Some(format!("{long_when}{i}")),
                )
            })
            .collect();
        let refs: Vec<(&str, &str, Option<&str>)> = entries
            .iter()
            .map(|(n, d, w)| (n.as_str(), d.as_str(), w.as_deref()))
            .collect();
        let set = catalog_set(&refs);
        let full = set.catalog(100_000);
        // Budgets count characters (see the catalog notes): assert in the same
        // units the budget is built in.
        assert!(
            full.chars().count() > 600,
            "full catalog should be long enough to force downgrades: {}",
            full.chars().count()
        );
        for budget in [600usize, 400, 200] {
            let catalog = set.catalog(budget);
            assert!(
                catalog.chars().count() <= budget,
                "budget {budget} exceeded: {} > {budget}",
                catalog.chars().count()
            );
            assert!(!catalog.is_empty());
        }
        // With a comfortable budget, when_to_use is shed before descriptions.
        let no_when_budget = set.render(false, None).chars().count() + 10;
        let catalog = set.catalog(no_when_budget);
        assert!(!catalog.contains("(when:"));
        assert!(catalog.contains(long_desc));
    }

    #[test]
    fn malformed_closing_fence_is_warning_skip() {
        let dir = tempfile::tempdir().unwrap();
        // Closing line with same-line content: invalid frontmatter — warn and
        // skip instead of polluting the body.
        write_skill(
            dir.path(),
            "badfence",
            "---\ndescription: Good skill\n--- junk",
            "Body",
        );
        let root = SkillRoot {
            source: SkillSource::User,
            dir: dir.path().to_path_buf(),
        };
        let discovery = discover(&[root]);
        assert!(discovery.set.is_empty());
        assert_eq!(discovery.warnings.len(), 1);
    }

    #[test]
    fn closing_fence_allows_trailing_whitespace() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(
            dir.path(),
            "ok",
            "---\ndescription: Good skill\n---   ",
            "Body",
        );
        let root = SkillRoot {
            source: SkillSource::User,
            dir: dir.path().to_path_buf(),
        };
        let discovery = discover(&[root]);
        assert!(discovery.warnings.is_empty(), "{:?}", discovery.warnings);
        assert_eq!(discovery.set.get("ok").unwrap().body, "Body");
    }

    #[test]
    fn crlf_closing_fence_parses() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("crlf");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\r\ndescription: Good skill\r\n---\r\nBody\r\n",
        )
        .unwrap();
        let root = SkillRoot {
            source: SkillSource::User,
            dir: dir.path().to_path_buf(),
        };
        let discovery = discover(&[root]);
        assert!(discovery.warnings.is_empty(), "{:?}", discovery.warnings);
        assert_eq!(discovery.set.get("crlf").unwrap().body, "Body");
    }

    /// Poll-based watching: `refresh()` picks up a newly added SKILL.md in
    /// an already-discovered root (deleted files drop on the next sweep).
    #[test]
    fn refresh_picks_up_new_skill_files() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(
            dir.path(),
            "first",
            "---\ndescription: First skill\n---",
            "Body one",
        );
        let root = SkillRoot {
            source: SkillSource::Project,
            dir: dir.path().to_path_buf(),
        };
        let mut discovery = discover(&[root]);
        assert_eq!(discovery.set.len(), 1);
        write_skill(
            dir.path(),
            "second",
            "---\ndescription: Second skill\n---",
            "Body two",
        );
        discovery.refresh();
        assert_eq!(discovery.set.len(), 2);
        assert!(discovery.set.get("second").is_some());
        // Deletions drop on the next sweep too.
        std::fs::remove_dir_all(dir.path().join("first")).unwrap();
        discovery.refresh();
        assert_eq!(discovery.set.len(), 1);
        assert!(discovery.set.get("first").is_none());
    }

    #[test]
    fn skill_set_replace_swaps_contents() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "a", "---\ndescription: Skill A\n---", "Body");
        let root = SkillRoot {
            source: SkillSource::Project,
            dir: dir.path().to_path_buf(),
        };
        let mut discovery = discover(&[root]);
        assert_eq!(discovery.set.len(), 1);
        discovery.set.replace(SkillSet::default());
        assert!(discovery.set.is_empty());
    }

    /// The hard-cut fallback never exceeds the budget at any quota (including
    /// tiny ones).
    #[test]
    fn catalog_never_exceeds_budget_even_when_tiny() {
        let set = catalog_set(&[(
            "a-very-long-skill-name",
            "A very long capability description, sized to fill the injection budget",
            Some("An equally long trigger-condition note"),
        )]);
        for budget in [1usize, 5, 10, 27, 28, 29, 40, 60] {
            let catalog = set.catalog(budget);
            assert!(
                catalog.chars().count() <= budget,
                "budget {budget} exceeded: {} > {budget}",
                catalog.chars().count()
            );
            assert!(!catalog.is_empty());
        }
    }
}
