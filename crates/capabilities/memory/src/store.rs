//! Persistent memory storage (SPEC section 7.2): a `MEMORY.md` index plus
//! four category entry files under `~/.wavecode/memories/`, in Markdown
//! bullet form (`- ` list items).
//!
//! Pure logic with sync IO: writes are single-entry appends (small files),
//! called by core's `memory_write` tool via `spawn_blocking`; reads happen
//! once at startup assembly. Everything is transparent to the user and
//! directly editable — this module only appends and never consolidates (the
//! SPEC 24h+5-session gated consolidation is a first-version simplification,
//! see the crate-level docs).

use std::path::{Path, PathBuf};

/// Index filename (at the memory root).
pub const INDEX_FILE: &str = "MEMORY.md";

/// Memory category (SPEC section 7.2, four kinds).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryCategory {
    /// User profile (preferences, habits, role).
    User,
    /// Feedback and corrections (the user's explicit "do this / never do that").
    Feedback,
    /// Project knowledge (repo conventions, architecture decisions, env facts).
    Project,
    /// External references (doc links, external-system notes).
    Reference,
}

impl MemoryCategory {
    /// All categories (fixed order, used for index grouping and test traversal).
    pub const ALL: [Self; 4] = [Self::User, Self::Feedback, Self::Project, Self::Reference];

    /// Category name (the `memory_write` tool parameter / extraction tag word).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Feedback => "feedback",
            Self::Project => "project",
            Self::Reference => "reference",
        }
    }

    /// Parse a category name; invalid values return None (callers turn them
    /// into business-failure output).
    /// Surrounding whitespace is trimmed before matching, so raw tool
    /// input like `" user "` still resolves (model output is untrusted).
    pub fn parse(raw: &str) -> Option<Self> {
        let normalized = raw.trim();
        Self::ALL.into_iter().find(|c| c.as_str() == normalized)
    }

    /// Category entry filename.
    pub fn file_name(self) -> &'static str {
        match self {
            Self::User => "user.md",
            Self::Feedback => "feedback.md",
            Self::Project => "project.md",
            Self::Reference => "reference.md",
        }
    }
}

/// Persistent memory store: an index plus category files under a root. The root
/// is injectable (tests use tempfile; production uses `~/.wavecode/memories/`).
#[derive(Debug, Clone)]
pub struct MemoryStore {
    root: PathBuf,
}

impl MemoryStore {
    /// Build with an explicit root (directories are created on first write,
    /// not here).
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Default root: `<home>/.wavecode/memories`.
    pub fn default_root(home: &Path) -> PathBuf {
        home.join(".wavecode").join("memories")
    }

    /// Storage root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Append one memory: the entry goes to the category file (a `- ` list
    /// item; multi-line content keeps continuation lines indented inside the
    /// same item), and the index gains one `- [category] summary` row.
    /// The summary is the content's first line truncated to 80 chars — the
    /// index is for navigation, the body lives in the category file.
    /// Blank content is rejected with an `InvalidInput` error before any
    /// directory or file is created, so junk `- ` bullets never pollute
    /// the category file or the index.
    pub fn append(&self, category: MemoryCategory, content: &str) -> std::io::Result<()> {
        if content.trim().is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "memory content must not be empty",
            ));
        }
        std::fs::create_dir_all(&self.root)?;
        let entry = format_entry(content);
        append_line(&self.root.join(category.file_name()), &entry)?;
        let summary = summarize(content);
        append_line(
            &self.root.join(INDEX_FILE),
            &format!(
                "- [{}] {summary} (see {})",
                category.as_str(),
                category.file_name()
            ),
        )
    }

    /// Read the whole index; a missing index returns an empty string (first
    /// use with no memories is a normal state, not an error).
    pub fn read_index(&self) -> std::io::Result<String> {
        match std::fs::read_to_string(self.root.join(INDEX_FILE)) {
            Ok(content) => Ok(content),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(e),
        }
    }

    /// Read one category's entries (loaded on demand; missing files return an
    /// empty string, same semantics as above).
    pub fn read_category(&self, category: MemoryCategory) -> std::io::Result<String> {
        match std::fs::read_to_string(self.root.join(category.file_name())) {
            Ok(content) => Ok(content),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(e),
        }
    }
}

/// Entry formatting: a `- ` list item; continuation lines of multi-line
/// content are indented two spaces to stay inside the same Markdown item.
fn format_entry(content: &str) -> String {
    let mut out = String::from("- ");
    out.push_str(&content.trim().replace('\n', "\n  "));
    out
}

/// Index summary: the content's first line, truncated to 80 chars by character
/// count (an ellipsis marks the cut).
fn summarize(content: &str) -> String {
    let first_line = content.trim().lines().next().unwrap_or_default();
    const MAX: usize = 80;
    if first_line.chars().count() <= MAX {
        first_line.to_owned()
    } else {
        let mut s: String = first_line.chars().take(MAX - 1).collect();
        s.push('…');
        s
    }
}

/// Append one line at the end of a file (creating the file if needed).
fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{line}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Append -> category file and index update together; a second append
    /// accumulates (the storage-side basis for cross-session recall).
    #[test]
    fn append_updates_category_file_and_index() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(dir.path().join("memories"));

        store.append(MemoryCategory::User, "prefers compact replies").unwrap();
        store.append(MemoryCategory::Project, "repo uses pnpm").unwrap();
        store.append(MemoryCategory::User, "long-time Rust user").unwrap();

        let user = store.read_category(MemoryCategory::User).unwrap();
        assert_eq!(user, "- prefers compact replies\n- long-time Rust user\n");
        let project = store.read_category(MemoryCategory::Project).unwrap();
        assert_eq!(project, "- repo uses pnpm\n");

        let index = store.read_index().unwrap();
        let lines: Vec<&str> = index.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "- [user] prefers compact replies (see user.md)");
        assert_eq!(lines[1], "- [project] repo uses pnpm (see project.md)");
        assert_eq!(lines[2], "- [user] long-time Rust user (see user.md)");
    }

    /// Multi-line content: continuation lines stay indented in the same item;
    /// the index summary takes only the first line.
    #[test]
    fn multiline_entry_stays_in_one_bullet() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(dir.path().to_path_buf());
        store
            .append(
                MemoryCategory::Feedback,
                "do not refactor working code\nunless asked to",
            )
            .unwrap();
        assert_eq!(
            store.read_category(MemoryCategory::Feedback).unwrap(),
            "- do not refactor working code\n  unless asked to\n"
        );
        let index = store.read_index().unwrap();
        assert!(index.contains("- [feedback] do not refactor working code (see feedback.md)"));
        assert!(!index.contains("unless asked to"), "index summary takes only the first line");
    }

    /// Empty store: index / category reads return empty strings, not errors
    /// (the first-use shape).
    #[test]
    fn empty_store_reads_as_empty_string() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(dir.path().join("nope"));
        assert_eq!(store.read_index().unwrap(), "");
        assert_eq!(store.read_category(MemoryCategory::Reference).unwrap(), "");
    }

    /// Category-name parsing roundtrip; invalid values rejected.
    #[test]
    fn category_parse_roundtrip() {
        for c in MemoryCategory::ALL {
            assert_eq!(MemoryCategory::parse(c.as_str()), Some(c));
        }
        assert_eq!(MemoryCategory::parse("nope"), None);
        assert_eq!(MemoryCategory::parse(""), None);
    }

    /// Whitespace-padded names still resolve (tool params come straight
    /// from untrusted model output); blank and unknown names are rejected.
    #[test]
    fn category_parse_trims_surrounding_whitespace() {
        assert_eq!(MemoryCategory::parse("  user "), Some(MemoryCategory::User));
        assert_eq!(
            MemoryCategory::parse("\tproject\n"),
            Some(MemoryCategory::Project)
        );
        assert_eq!(MemoryCategory::parse("   "), None);
        assert_eq!(MemoryCategory::parse(""), None);
    }

    /// Blank content is rejected (InvalidInput) with no filesystem side
    /// effects: no junk `- ` bullets in the category file or the index.
    #[test]
    fn append_rejects_blank_content_without_side_effects() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("memories");
        let store = MemoryStore::new(root.clone());

        for blank in ["", "   ", "\n  \n"] {
            let err = store.append(MemoryCategory::User, blank).unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        }
        assert!(
            !root.exists(),
            "rejected appends must not create the store dir"
        );

        // The guard only intercepts blanks; valid writes still work.
        store.append(MemoryCategory::User, "prefers compact replies").unwrap();
        assert_eq!(
            store.read_category(MemoryCategory::User).unwrap(),
            "- prefers compact replies\n"
        );
    }

    /// A long first line is truncated to 80 chars with an ellipsis.
    #[test]
    fn long_summary_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(dir.path().to_path_buf());
        let long = "x".repeat(200);
        store.append(MemoryCategory::User, &long).unwrap();
        let index = store.read_index().unwrap();
        let line = index.trim_end();
        // Summary truncated to 80 chars (79 + ellipsis), followed by the
        // category-file pointer.
        assert!(line.contains('…'), "cut should add an ellipsis: {line}");
        assert!(line.ends_with("(see user.md)"));
        let summary = line
            .trim_start_matches("- [user] ")
            .trim_end_matches("(see user.md)")
            .trim_end();
        assert_eq!(summary.chars().count(), 80);
    }
}
