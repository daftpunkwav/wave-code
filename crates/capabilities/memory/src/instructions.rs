//! Instruction memory (`AGENTS.md`) discovery, concatenation, and `@path`
//! reference expansion.
//!
//! Pure logic with sync IO: collection is a one-shot startup action (cli
//! bootstrap), off the turn loop's hot path, so it uses `std::fs` directly
//! and is 100% unit-testable (tempfile-built directory trees).
//!
//! Collection order (concatenated global-first, local-last):
//!
//! ```text
//! user-level ~/.wavecode/AGENTS.md -> project-root AGENTS.md + .wavecode/rules/*.md
//! -> cwd AGENTS.md + .wavecode/rules/*.md
//! ```
//!
//! The project root is located upward via `.git` (a directory or a file, so
//! worktree shapes work); when the cwd equals the project root or lies outside
//! it, real paths are deduplicated and no file is concatenated twice.
//!
//! Each tier also concatenates `AGENTS.local.md` right after its `AGENTS.md`
//! when present — the personal, uncommitted supplement. There are no fallback
//! filenames: a directory without `AGENTS.md` simply has no instruction tier.

use std::path::{Path, PathBuf};

/// Instruction memory filename.
pub const INSTRUCTION_FILE: &str = "AGENTS.md";

/// Per-directory local supplement, concatenated right after
/// [`INSTRUCTION_FILE`] when present: personal additions that stay
/// uncommitted, mirroring the `*.local.md` ignore convention.
pub const LOCAL_INSTRUCTION_FILE: &str = "AGENTS.local.md";

/// The instruction files of one tier directory, in concat order: `AGENTS.md`
/// followed by `AGENTS.local.md` when both exist. The local file is a
/// supplement, not a substitute: without `AGENTS.md` the tier is skipped
/// entirely — a stray local file invents no instructions.
fn resolve_instruction_files(dir: &Path) -> Vec<PathBuf> {
    let base = dir.join(INSTRUCTION_FILE);
    if !base.exists() {
        return Vec::new();
    }
    let mut files = vec![base];
    let local = dir.join(LOCAL_INSTRUCTION_FILE);
    if local.exists() {
        files.push(local);
    }
    files
}

/// Depth cap for recursive `@path` reference expansion:
/// AGENTS.md itself is depth 0, files it references are depth 1, and so on;
/// references inside files past the cap are kept as literal text, unexpanded.
pub const MAX_INCLUDE_DEPTH: usize = 5;

/// Instruction memory collection result.
#[derive(Debug, Clone, Default)]
pub struct InstructionMemory {
    /// Concatenated output (global first, local last, one titled section per
    /// source); empty when there is no content.
    pub combined: String,
    /// Source files that went into the concatenation (in concat order; for
    /// debugging and display).
    pub sources: Vec<PathBuf>,
}

/// Locate the project root upward: from `cwd`, take the first ancestor
/// containing `.git` (a directory or a file — worktrees use a file); return
/// None when there is none (a non-repo environment collects only the
/// user-level and cwd tiers).
pub fn find_project_root(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors()
        .find(|dir| dir.join(".git").exists())
        .map(Path::to_path_buf)
}

/// Collect instruction memory: user level (`home/.wavecode/AGENTS.md`) ->
/// project root -> cwd, concatenated tier by tier; the project-root and cwd
/// tiers each bring their `.wavecode/rules/*.md` files (merged sorted by
/// filename). A None `home` skips the user level. Every file is concatenated
/// after `@path` reference expansion; unreadable files are silently skipped
/// (memory is an enhancement — a missing file must not block startup).
pub fn collect(home: Option<&Path>, cwd: &Path) -> InstructionMemory {
    let mut mem = InstructionMemory::default();
    let mut seen: Vec<PathBuf> = Vec::new();

    // One tier: its instruction files (AGENTS.md + AGENTS.local.md, when
    // present) plus that tier's rules-dir *.md files (sorted by filename).
    // The user and project tiers have different directory shapes
    // (~/.wavecode/AGENTS.md + ~/.wavecode/rules vs
    // <dir>/AGENTS.md + <dir>/.wavecode/rules), so the caller passes both
    // paths explicitly.
    let collect_level = |instr_dir: PathBuf,
                         rules_dir: PathBuf,
                         mem: &mut InstructionMemory,
                         seen: &mut Vec<PathBuf>| {
        let mut files = resolve_instruction_files(&instr_dir);
        if let Ok(entries) = std::fs::read_dir(&rules_dir) {
            let mut rules: Vec<PathBuf> = entries
                .filter_map(std::result::Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|ext| ext == "md"))
                .collect();
            rules.sort();
            files.extend(rules);
        }
        for file in files {
            // Dedup: with shapes like cwd == project root the same file is
            // concatenated only once (compared by canonicalized path, falling
            // back to the raw path on failure — the comparison only needs to
            // be stable, not truly resolved).
            let key = std::fs::canonicalize(&file).unwrap_or_else(|_| file.clone());
            if seen.contains(&key) {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&file) else {
                continue; // Missing / unreadable file: skip.
            };
            seen.push(key);
            let base = file.parent().map(Path::to_path_buf).unwrap_or_default();
            let mut nested: Vec<PathBuf> = Vec::new();
            let expanded = expand_at_refs(&content, &base, 0, &mut Vec::new(), &mut nested);
            if !mem.combined.is_empty() {
                mem.combined.push_str("\n\n");
            }
            mem.combined.push_str(&format!("## {}\n\n", file.display()));
            mem.combined.push_str(expanded.trim_end());
            mem.sources.push(file);
            // @ref-expanded files ride along: `/memory` must list every
            // file whose content was injected, not just the tier heads.
            mem.sources.extend(nested);
        }
    };

    if let Some(home) = home {
        let user_dir = home.join(".wavecode");
        collect_level(
            user_dir.clone(),
            user_dir.join("rules"),
            &mut mem,
            &mut seen,
        );
    }
    if let Some(root) = find_project_root(cwd) {
        collect_level(
            root.clone(),
            root.join(".wavecode").join("rules"),
            &mut mem,
            &mut seen,
        );
    }
    collect_level(
        cwd.to_path_buf(),
        cwd.join(".wavecode").join("rules"),
        &mut mem,
        &mut seen,
    );
    mem
}

/// Expand `@path` references in content: each referenced file's content
/// replaces the marker in place (with a source title), recursing up to
/// [`MAX_INCLUDE_DEPTH`]; `visited` records already-expanded files
/// (canonicalized paths) so repeated / cyclic references stay literal
/// (cycle-safe, duplicate-safe). Every successfully expanded file is
/// appended to `expanded` (raw paths, in expansion order) so the caller
/// can account for the injected content. References to missing /
/// unreadable files are likewise kept literal — shown honestly, never
/// silently dropped.
fn expand_at_refs(
    content: &str,
    base_dir: &Path,
    depth: usize,
    visited: &mut Vec<PathBuf>,
    expanded: &mut Vec<PathBuf>,
) -> String {
    let mut out = String::with_capacity(content.len());
    for token in content.split_inclusive(char::is_whitespace) {
        let (body, trail_ws) = split_trailing_whitespace(token);
        match parse_at_ref(body) {
            Some(reference) if at_ref_allowed(reference) => {
                let path = base_dir.join(reference);
                // Symlink boundary: the lexical check (at_ref_allowed) cannot
                // stop soft links — `@link.md` may point outside the cwd.
                // Assert the prefix after canonicalization; when the target is
                // missing (canonicalize fails) the read would fail too, so
                // keeping it literal is the same outcome.
                let in_bounds = canonicalized_in_bounds(&path, base_dir);
                let key = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
                let expanded_text =
                    if in_bounds && depth < MAX_INCLUDE_DEPTH && !visited.contains(&key) {
                        std::fs::read_to_string(&path).ok().map(|inner| {
                            visited.push(key);
                            expanded.push(path.clone());
                            let inner_base = path.parent().unwrap_or(base_dir);
                            let inner =
                                expand_at_refs(&inner, inner_base, depth + 1, visited, expanded);
                            format!("### {reference}\n\n{}", inner.trim_end())
                        })
                    } else {
                        None // Out of bounds (incl. link escape) / over depth /
                        // already expanded (cycle): keep literal.
                    };
                match expanded_text {
                    Some(text) => {
                        out.push_str(&text);
                        out.push_str(trail_ws);
                    }
                    None => out.push_str(token),
                }
            }
            // Non-reference markers, or references rejected by the trust
            // boundary: always kept literal.
            _ => out.push_str(token),
        }
    }
    out
}

/// Split a token's trailing whitespace (the separator kept by
/// `split_inclusive`), returning (body, trailing whitespace).
fn split_trailing_whitespace(token: &str) -> (&str, &str) {
    let body = token.trim_end();
    (body, &token[body.len()..])
}

/// Trust boundary for `@ref`: only relative paths inside base_dir. Absolute
/// paths (including Windows drive / UNC prefixes and rooted forms) and `..`
/// components are always rejected — an unbounded reference is an unsandboxed
/// arbitrary-file-read primitive (a cloned, untrusted repo's AGENTS.md could
/// pull files from outside the cwd into model context). Rejected references
/// follow the same policy as missing files: kept literal, shown honestly.
fn at_ref_allowed(reference: &str) -> bool {
    use std::path::Component;
    let path = Path::new(reference);
    if path.is_absolute() {
        return false;
    }
    path.components()
        .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

/// Boundary assertion on canonicalized paths: `path` (the candidate file after
/// resolving `@ref`) must canonicalize to somewhere still inside canonicalized
/// `base_dir`. A second layer above the lexical check: symlink components only
/// resolve outside base_dir after canonicalization. Either side failing to
/// canonicalize (typically: the target does not exist) returns false — the
/// read would fail anyway, homomorphic with keeping it literal.
fn canonicalized_in_bounds(path: &Path, base_dir: &Path) -> bool {
    match (std::fs::canonicalize(path), std::fs::canonicalize(base_dir)) {
        (Ok(canon), Ok(base)) => canon.starts_with(&base),
        _ => false,
    }
}

/// Parse an `@path` reference marker: starts with `@`, followed by a non-empty
/// path; strip common trailing punctuation (`.` `,` `;` `:` `)` `]`). Paths
/// containing whitespace other than `@` are never valid (tokens are already
/// split on whitespace, so this holds naturally). Non-references return None.
fn parse_at_ref(token: &str) -> Option<&str> {
    let body = token.strip_prefix('@')?;
    let path = body.trim_end_matches(['.', ',', ';', ':', ')', ']']);
    if path.is_empty() {
        return None;
    }
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    /// Concat order: user level -> project root -> cwd,
    /// global first.
    #[test]
    fn concat_order_global_first() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let root = dir.path().join("repo");
        let cwd = root.join("crates/xyz");
        write(&home.join(".wavecode/AGENTS.md"), "USER-LEVEL");
        write(&root.join(".git/HEAD"), "ref: refs/heads/main\n");
        write(&root.join("AGENTS.md"), "PROJECT-ROOT");
        write(&cwd.join("AGENTS.md"), "CWD-LEVEL");

        let mem = collect(Some(&home), &cwd);
        let (u, r, c) = (
            mem.combined.find("USER-LEVEL").unwrap(),
            mem.combined.find("PROJECT-ROOT").unwrap(),
            mem.combined.find("CWD-LEVEL").unwrap(),
        );
        assert!(
            u < r && r < c,
            "concat order should be user-level -> project-root -> cwd:\n{}",
            mem.combined
        );
        assert_eq!(mem.sources.len(), 3);
    }

    /// Project root location: nested cwd searches upward for `.git`; when
    /// cwd == project root the file is not concatenated twice.
    #[test]
    fn project_root_detection_and_dedup() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        write(&root.join(".git/HEAD"), "ref: refs/heads/main\n");
        write(&root.join("AGENTS.md"), "ROOT");
        let nested = root.join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(find_project_root(&nested).as_deref(), Some(root.as_path()));
        // cwd == project root: the same AGENTS.md is concatenated once.
        let mem = collect(None, &root);
        assert_eq!(mem.combined.matches("ROOT").count(), 1);
        assert_eq!(mem.sources.len(), 1);
    }

    /// @-reference expansion: basic substitution + directory-relative file
    /// resolution.
    #[test]
    fn at_ref_expansion_basic() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        write(&root.join(".git/HEAD"), "x\n");
        write(&root.join("docs/extra.md"), "EXTRA-CONTENT");
        write(&root.join("AGENTS.md"), "before\n@docs/extra.md\nafter");

        let mem = collect(None, &root);
        assert!(
            mem.combined.contains("EXTRA-CONTENT"),
            "reference should expand:\n{}",
            mem.combined
        );
        assert!(
            !mem.combined.contains("@docs/extra.md"),
            "marker should be replaced"
        );
        // References to missing files stay literal (honest display).
        write(&root.join("AGENTS.md"), "see @docs/missing.md for details");
        let mem = collect(None, &root);
        assert!(mem.combined.contains("@docs/missing.md"));
    }

    /// Trust boundary: references with absolute paths or `..` components never
    /// expand (they would pull arbitrary files from outside the cwd into model
    /// context) and stay literal; whitelisted relative forms (`./x`) still
    /// expand.
    #[test]
    fn at_ref_outside_base_dir_is_not_expanded() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        write(&root.join("docs/extra.md"), "EXTRA-CONTENT");
        write(&dir.path().join("secret.md"), "SECRET-CONTENT");
        write(
            &root.join("AGENTS.md"),
            "absolute @D:/secret.md and parent @../secret.md never expand, but @./docs/extra.md does",
        );

        let mem = collect(None, &root);
        assert!(!mem.combined.contains("SECRET-CONTENT"), "{}", mem.combined);
        assert!(mem.combined.contains("@D:/secret.md"));
        assert!(mem.combined.contains("@../secret.md"));
        assert!(
            mem.combined.contains("EXTRA-CONTENT"),
            "./ relative refs should expand"
        );
    }

    /// Symlink boundary (unix): the lexical check cannot stop link
    /// components — `@link.md` may point at a file outside base_dir. After the
    /// canonicalized-prefix assertion, out-of-bounds links stay literal while
    /// in-bounds links still expand (regression lock).
    #[cfg(unix)]
    #[test]
    fn at_ref_symlink_escaping_base_dir_is_not_expanded() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        write(&root.join(".git/HEAD"), "x\n");
        write(&dir.path().join("secret.md"), "SECRET-CONTENT");
        write(&root.join("docs/real.md"), "REAL-CONTENT");
        symlink(dir.path().join("secret.md"), root.join("docs/link.md")).unwrap();
        symlink(root.join("docs/real.md"), root.join("docs/in-link.md")).unwrap();
        write(
            &root.join("AGENTS.md"),
            "out-of-bounds @docs/link.md and in-bounds @docs/in-link.md",
        );

        let mem = collect(None, &root);
        assert!(
            !mem.combined.contains("SECRET-CONTENT"),
            "escaping link content must not enter context:\n{}",
            mem.combined
        );
        assert!(
            mem.combined.contains("@docs/link.md"),
            "escaping links stay literal:\n{}",
            mem.combined
        );
        assert!(
            mem.combined.contains("REAL-CONTENT"),
            "in-bounds links still expand:\n{}",
            mem.combined
        );
    }

    /// @-reference depth cap: a chained f0->f1->…->f7 leaves
    /// references past depth 5 unexpanded, kept literal.
    #[test]
    fn at_ref_expansion_depth_limit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        write(&root.join(".git/HEAD"), "x\n");
        for i in 0..=7 {
            let content = if i == 7 {
                format!("LEAF-{i}")
            } else {
                format!("LV{i}\n@f{}.md", i + 1)
            };
            write(&root.join(format!("f{i}.md")), &content);
        }
        write(&root.join("AGENTS.md"), "@f0.md");

        let mem = collect(None, &root);
        // AGENTS.md is depth 0 -> f0..f4 (depths 1..=5) expand; the @f5.md
        // inside f4 (depth 5) already hits the cap and stays literal.
        for i in 0..=4 {
            assert!(
                mem.combined.contains(&format!("LV{i}")),
                "LV{i} should expand:\n{}",
                mem.combined
            );
        }
        assert!(
            mem.combined.contains("@f5.md"),
            "capped references stay literal:\n{}",
            mem.combined
        );
        assert!(
            !mem.combined.contains("LV5"),
            "depth-6 content must not appear"
        );
    }

    /// @-reference cycle guard: mutual a <-> b references must
    /// terminate; re-references to already-expanded files stay literal (visited
    /// dedup).
    #[test]
    fn at_ref_expansion_cycle_terminates() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        write(&root.join(".git/HEAD"), "x\n");
        write(&root.join("a.md"), "A-CONTENT\n@b.md");
        write(&root.join("b.md"), "B-CONTENT\n@a.md");
        write(&root.join("AGENTS.md"), "@a.md");

        let mem = collect(None, &root);
        assert!(mem.combined.contains("A-CONTENT"));
        assert!(mem.combined.contains("B-CONTENT"));
        // The back reference to a inside b stays literal (a is in visited).
        assert!(mem.combined.contains("@a.md"));
    }

    /// Successfully expanded `@ref` targets register as sources: `/memory`
    /// must list every file whose content was injected, while references
    /// kept literal (missing or trust-boundary-rejected; they inject
    /// nothing) must not appear.
    #[test]
    fn expanded_refs_register_as_sources() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        write(&root.join(".git/HEAD"), "x\n");
        write(&root.join("docs/extra.md"), "EXTRA-CONTENT");
        write(&root.join("f0.md"), "LV0\n@f1.md");
        write(&root.join("f1.md"), "LV1");
        write(&dir.path().join("outside.md"), "OUTSIDE-CONTENT");
        write(
            &root.join("AGENTS.md"),
            "see @docs/extra.md and @f0.md, but @docs/missing.md and @../outside.md stay literal",
        );

        let mem = collect(None, &root);
        let names: Vec<String> = mem
            .sources
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        // AGENTS.md first, then its expanded refs in expansion order.
        assert_eq!(names[0], "AGENTS.md", "{names:?}");
        assert!(names.contains(&"extra.md".to_string()), "{names:?}");
        assert!(names.contains(&"f0.md".to_string()), "{names:?}");
        assert!(names.contains(&"f1.md".to_string()), "{names:?}");
        // Injected content really came from those files.
        assert!(mem.combined.contains("EXTRA-CONTENT"));
        assert!(mem.combined.contains("LV1"));
        // Kept-literal references register nothing: the missing file and
        // the trust-rejected parent escape inject no content.
        assert!(!names.contains(&"missing.md".to_string()), "{names:?}");
        assert!(
            !mem.combined.contains("OUTSIDE-CONTENT"),
            "{}",
            mem.combined
        );
    }

    /// Rules-dir merge `.wavecode/rules/*.md` concatenated
    /// sorted by filename.
    #[test]
    fn rules_dir_merged_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        write(&root.join(".git/HEAD"), "x\n");
        write(&root.join("AGENTS.md"), "ROOT");
        write(&root.join(".wavecode/rules/02-style.md"), "RULE-STYLE");
        write(&root.join(".wavecode/rules/01-test.md"), "RULE-TEST");
        write(&root.join(".wavecode/rules/skip.txt"), "NOT-MD");

        let mem = collect(None, &root);
        let (t, s) = (
            mem.combined.find("RULE-TEST").unwrap(),
            mem.combined.find("RULE-STYLE").unwrap(),
        );
        assert!(
            t < s,
            "rules should merge sorted by filename:\n{}",
            mem.combined
        );
        assert!(
            !mem.combined.contains("NOT-MD"),
            "non-.md files do not merge"
        );
        // Sources: AGENTS.md plus two rules files.
        assert_eq!(mem.sources.len(), 3);
    }

    /// Non-repo environment (no .git): only the user-level and cwd tiers are
    /// collected.
    #[test]
    fn non_repo_collects_user_and_cwd_only() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let cwd = dir.path().join("plain/sub");
        write(&home.join(".wavecode/AGENTS.md"), "USER-LEVEL");
        write(&cwd.join("AGENTS.md"), "CWD-LEVEL");
        // A tempdir ancestor that happens to be a git repo would mislocate —
        // confirm explicitly before asserting.
        if cwd.ancestors().all(|p| !p.join(".git").exists()) {
            let mem = collect(Some(&home), &cwd);
            assert!(mem.combined.contains("USER-LEVEL"));
            assert!(mem.combined.contains("CWD-LEVEL"));
            assert_eq!(mem.sources.len(), 2);
        }
    }

    /// The user-level rules dir is `~/.wavecode/rules/*.md` (unlike the
    /// project-tier `<dir>/.wavecode/rules` shape — regression lock; it was
    /// once miscomputed as `~/.wavecode/.wavecode/rules`).
    #[test]
    fn user_level_rules_dir() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let cwd = dir.path().join("plain");
        write(&home.join(".wavecode/AGENTS.md"), "USER-LEVEL");
        write(&home.join(".wavecode/rules/01-global.md"), "GLOBAL-RULE");
        write(&cwd.join("AGENTS.md"), "CWD-LEVEL");
        if cwd.ancestors().all(|p| !p.join(".git").exists()) {
            let mem = collect(Some(&home), &cwd);
            assert!(
                mem.combined.contains("GLOBAL-RULE"),
                "user-level rules should merge:\n{}",
                mem.combined
            );
            let (u, g) = (
                mem.combined.find("USER-LEVEL").unwrap(),
                mem.combined.find("GLOBAL-RULE").unwrap(),
            );
            assert!(u < g, "user-level rules sort after their own AGENTS.md");
        }
    }

    /// Local supplements: `AGENTS.local.md` concatenates right after the
    /// tier's `AGENTS.md`; a directory without `AGENTS.md` has no tier at
    /// all (no fallback filenames exist).
    #[test]
    fn local_file_appends_after_the_tier_instruction_file() {
        let dir = tempfile::tempdir().unwrap();

        // AGENTS.md alone: the tier loads from it.
        let root_a = dir.path().join("repo-a");
        write(&root_a.join(".git/HEAD"), "x\n");
        write(&root_a.join("AGENTS.md"), "A-AGENTS");
        let mem = collect(None, &root_a);
        assert!(
            mem.combined.contains("A-AGENTS"),
            "AGENTS.md should load:\n{}",
            mem.combined
        );

        // AGENTS.md + AGENTS.local.md: local lands after, both load.
        let root_b = dir.path().join("repo-b");
        write(&root_b.join(".git/HEAD"), "x\n");
        write(&root_b.join("AGENTS.md"), "B-AGENTS");
        write(&root_b.join("AGENTS.local.md"), "B-LOCAL");
        let mem = collect(None, &root_b);
        let (base, local) = (
            mem.combined.find("B-AGENTS").unwrap(),
            mem.combined.find("B-LOCAL").unwrap(),
        );
        assert!(
            base < local,
            "local sorts after AGENTS.md:\n{}",
            mem.combined
        );
        assert_eq!(mem.sources.len(), 2);

        // AGENTS.local.md alone: no AGENTS.md, no tier — a stray local file
        // does not invent instructions.
        let root_c = dir.path().join("repo-c");
        write(&root_c.join(".git/HEAD"), "x\n");
        write(&root_c.join("AGENTS.local.md"), "C-LOCAL");
        let mem = collect(None, &root_c);
        assert!(
            !mem.combined.contains("C-LOCAL"),
            "local without AGENTS.md must not load:\n{}",
            mem.combined
        );

        // Legacy filenames are dead: neither WAVECODE.md nor CLAUDE.md
        // is read anymore.
        let root_d = dir.path().join("repo-d");
        write(&root_d.join(".git/HEAD"), "x\n");
        write(&root_d.join("WAVECODE.md"), "D-WAVECODE");
        write(&root_d.join("CLAUDE.md"), "D-CLAUDE");
        let mem = collect(None, &root_d);
        assert!(!mem.combined.contains("D-WAVECODE"), "{}", mem.combined);
        assert!(!mem.combined.contains("D-CLAUDE"), "{}", mem.combined);
        assert!(mem.sources.is_empty());
    }
}
