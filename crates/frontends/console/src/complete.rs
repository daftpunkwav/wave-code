//! Completion providers for the console: slash commands and `@` file
//! mentions, combined into one provider for the editor.

use std::path::Path;

use tui_engine::autocomplete::{Completion, CompletionProvider, FuzzyProvider, TriggerKind};
use tui_engine::fuzzy;

/// File-scan bounds (reference parity): inventory cap and suggestion cap.
pub const MAX_SCANNED: usize = 2000;
pub const MAX_SUGGESTIONS: usize = 50;

/// Directories never inventoried for `@` mentions.
const SKIPPED_DIRS: [&str; 5] = [".git", "target", "node_modules", ".venv", "dist"];

/// A bounded recursive inventory of the workspace for `@` mentions.
pub struct FileInventory {
    /// Repo-relative (or absolute-free) paths, sorted.
    paths: Vec<String>,
}

impl FileInventory {
    /// Walk `root` breadth-first, bounded by [`MAX_SCANNED`] entries.
    /// Directories in [`SKIPPED_DIRS`] and hidden entries are skipped.
    pub fn scan(root: &Path) -> Self {
        let mut paths = Vec::new();
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(root.to_path_buf());
        while let Some(dir) = queue.pop_front() {
            let Ok(read_dir) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in read_dir.flatten() {
                if paths.len() >= MAX_SCANNED {
                    return Self { paths };
                }
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') {
                    continue;
                }
                let path = dir.join(&name);
                let rel = path
                    .strip_prefix(root)
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
                    .unwrap_or(name.clone());
                if file_type.is_dir() {
                    if SKIPPED_DIRS.contains(&name.as_str()) {
                        continue;
                    }
                    queue.push_back(path);
                } else {
                    paths.push(rel);
                }
            }
        }
        paths.sort();
        Self { paths }
    }

    /// An empty inventory (non-workspace sessions).
    pub fn empty() -> Self {
        Self { paths: Vec::new() }
    }

    /// The inventoried paths.
    pub fn paths(&self) -> &[String] {
        &self.paths
    }
}

/// Provider over slash commands and `@` file mentions.
pub struct ConsoleProvider {
    slash: FuzzyProvider,
    inventory: FileInventory,
    /// Model picker aliases for `/model <name>` argument completion:
    /// (alias, provider description).
    models: Vec<(String, String)>,
    /// Custom theme names for `/theme <name>` argument completion.
    themes: Vec<String>,
}

impl ConsoleProvider {
    /// A provider over the given command names and file inventory.
    pub fn new(command_names: &[String], inventory: FileInventory) -> Self {
        let candidates = command_names
            .iter()
            .map(|name| Completion {
                label: format!("/{name}"),
                description: None,
                insert: format!("/{name} "),
            })
            .collect();
        Self {
            slash: FuzzyProvider::new(candidates),
            inventory,
            models: Vec::new(),
            themes: Vec::new(),
        }
    }

    /// Offer these models for `/model <name>` argument completion.
    pub fn with_models(mut self, models: Vec<(String, String)>) -> Self {
        self.models = models;
        self
    }

    /// Offer these custom theme names for `/theme <name>` argument
    /// completion.
    pub fn with_themes(mut self, themes: Vec<String>) -> Self {
        self.themes = themes;
        self
    }

    /// Argument candidates for one command after its name: scored
    /// against the partial argument, each inserting the full line
    /// (the editor replaces from the `/` trigger).
    fn argument_completions(&self, command: &str, arg: &str) -> Vec<Completion> {
        let statics: &[(&str, &str)] = match command {
            "effort" => &[
                ("off", "clear the effort parameter"),
                ("low", ""),
                ("medium", ""),
                ("high", ""),
            ],
            "theme" => &[("light", ""), ("dark", ""), ("auto", "follow the terminal")],
            "permissions" => &[("plan", ""), ("auto", ""), ("wave", "")],
            _ => &[],
        };
        let mut candidates: Vec<Completion> = statics
            .iter()
            .map(|(label, description)| Completion {
                label: label.to_string(),
                description: (!description.is_empty()).then(|| description.to_string()),
                insert: format!("/{command} {label}"),
            })
            .collect();
        if command == "model" {
            candidates.extend(self.models.iter().map(|(alias, provider)| Completion {
                label: alias.clone(),
                description: Some(provider.clone()),
                insert: format!("/{command} {alias}"),
            }));
        }
        if command == "theme" {
            candidates.extend(self.themes.iter().map(|name| Completion {
                label: name.clone(),
                description: Some("custom".to_string()),
                insert: format!("/{command} {name}"),
            }));
        }
        let mut scored: Vec<(i64, Completion)> = candidates
            .into_iter()
            .filter_map(|completion| fuzzy::score(arg, &completion.label).map(|s| (s, completion)))
            .collect();
        scored.sort_by_key(|(score, _)| *score);
        scored
            .into_iter()
            .take(MAX_SUGGESTIONS)
            .map(|(_, completion)| completion)
            .collect()
    }
}

impl CompletionProvider for ConsoleProvider {
    fn complete(&self, kind: TriggerKind, token: &str) -> Vec<Completion> {
        match kind {
            TriggerKind::Slash => {
                // `model fac` past the first space is argument
                // completion for the named command.
                if let Some((command, arg)) = token.split_once(' ') {
                    return self.argument_completions(command, arg);
                }
                self.slash.complete(kind, token)
            }
            TriggerKind::Mention => {
                let mut scored: Vec<(i64, &String)> = self
                    .inventory
                    .paths()
                    .iter()
                    .filter_map(|path| fuzzy::score(token, path).map(|s| (s, path)))
                    .collect();
                scored.sort_by_key(|(s, _)| *s);
                scored
                    .into_iter()
                    .take(MAX_SUGGESTIONS)
                    .map(|(_, path)| Completion {
                        label: path.clone(),
                        description: None,
                        insert: format!("{path} "),
                    })
                    .collect()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inventory_skips_hidden_and_vendored_dirs() {
        let temp = std::env::temp_dir().join(format!("wc-inv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(temp.join("src")).unwrap();
        std::fs::create_dir_all(temp.join(".git")).unwrap();
        std::fs::create_dir_all(temp.join("target")).unwrap();
        std::fs::write(temp.join("src/lib.rs"), "").unwrap();
        std::fs::write(temp.join(".git/config"), "").unwrap();
        std::fs::write(temp.join("target/out.bin"), "").unwrap();

        let inventory = FileInventory::scan(&temp);
        let paths = inventory.paths();
        assert_eq!(paths, vec!["src/lib.rs".to_string()]);
        let _ = std::fs::remove_dir_all(&temp);
    }

    #[test]
    fn mention_completions_rank_matches() {
        let temp = std::env::temp_dir().join(format!("wc-inv2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        std::fs::write(temp.join("main.rs"), "").unwrap();
        std::fs::write(temp.join("readme.md"), "").unwrap();

        let provider = ConsoleProvider::new(&["help".to_string()], FileInventory::scan(&temp));
        let completions = provider.complete(TriggerKind::Mention, "main");
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].label, "main.rs");
        let _ = std::fs::remove_dir_all(&temp);
    }

    #[test]
    fn slash_completions_still_work_through_combined_provider() {
        let provider = ConsoleProvider::new(
            &["help".to_string(), "model".to_string()],
            FileInventory::empty(),
        );
        let completions = provider.complete(TriggerKind::Slash, "he");
        assert_eq!(completions[0].label, "/help");
    }

    #[test]
    fn effort_arguments_complete_with_the_full_line() {
        let provider = ConsoleProvider::new(&[], FileInventory::empty());
        let completions = provider.complete(TriggerKind::Slash, "effort hi");
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].label, "high");
        assert_eq!(completions[0].insert, "/effort high");
    }

    #[test]
    fn model_arguments_come_from_the_catalog() {
        let provider = ConsoleProvider::new(&[], FileInventory::empty())
            .with_models(vec![("fast-model".to_string(), "openai".to_string())]);
        let completions = provider.complete(TriggerKind::Slash, "model fas");
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].label, "fast-model");
        assert_eq!(completions[0].description.as_deref(), Some("openai"));
        assert_eq!(completions[0].insert, "/model fast-model");
    }

    #[test]
    fn unknown_command_arguments_complete_to_nothing() {
        let provider = ConsoleProvider::new(&[], FileInventory::empty());
        assert!(provider.complete(TriggerKind::Slash, "help x").is_empty());
    }

    #[test]
    fn theme_arguments_list_the_three_modes() {
        let provider = ConsoleProvider::new(&[], FileInventory::empty());
        // Fuzzy subsequence matching: "a" ranks "auto" but also "dark".
        let completions = provider.complete(TriggerKind::Slash, "theme a");
        let labels: Vec<&str> = completions.iter().map(|c| c.label.as_str()).collect();
        assert!(labels.contains(&"auto"), "{labels:?}");
        assert!(labels.contains(&"dark"), "{labels:?}");
        // The exact prefix wins the top slot.
        assert_eq!(completions[0].label, "auto");
        assert_eq!(completions[0].insert, "/theme auto");
    }

    #[test]
    fn theme_arguments_include_custom_theme_names() {
        let provider = ConsoleProvider::new(&[], FileInventory::empty())
            .with_themes(vec!["sunset".to_string()]);
        let completions = provider.complete(TriggerKind::Slash, "theme sun");
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].label, "sunset");
        assert_eq!(completions[0].description.as_deref(), Some("custom"));
        assert_eq!(completions[0].insert, "/theme sunset");
    }
}
