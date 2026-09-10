/*!
 * @file UserProfile
 * @description User identity and environment snapshots for prompts.
 *
 * Responsibilities:
 * - Carry explicit user preferences into prompt assembly.
 * - Snapshot the execution environment once per session.
 * - Render both as stable markdown blocks.
 *
 * This module must not depend on: any other workspace crate. Snapshots
 * are taken by explicit constructors, never ambiently.
 */

//! Profile and environment as caller-owned data.

use std::collections::HashMap;
use std::path::PathBuf;

/// Declared user preferences.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserProfile {
    /// Display name used in transcripts.
    pub name: String,
    /// Contact email, if the user shared one.
    pub email: Option<String>,
    /// IANA timezone for time-sensitive answers.
    pub timezone: Option<String>,
    /// Free-form preference keys and values.
    pub preferences: HashMap<String, String>,
}

impl UserProfile {
    /// Render the profile as a prompt block; empty when anonymous.
    pub fn prompt_block(&self) -> String {
        if self.name.trim().is_empty() && self.preferences.is_empty() {
            return String::new();
        }
        let mut out = String::from("## User\n");
        if !self.name.trim().is_empty() {
            out.push_str(&format!("Name: {}\n", self.name.trim()));
        }
        if let Some(email) = &self.email {
            out.push_str(&format!("Email: {email}\n"));
        }
        if let Some(timezone) = &self.timezone {
            out.push_str(&format!("Timezone: {timezone}\n"));
        }
        let mut keys: Vec<&String> = self.preferences.keys().collect();
        keys.sort();
        for key in keys {
            out.push_str(&format!("{}: {}\n", key, self.preferences[key]));
        }
        out
    }
}

/// Execution environment snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentSnapshot {
    /// Operating system from the build target.
    pub os: String,
    /// CPU architecture from the build target.
    pub arch: String,
    /// Working directory at snapshot time.
    pub cwd: PathBuf,
    /// Login shell, if discoverable from the environment.
    pub shell: Option<String>,
}

impl EnvironmentSnapshot {
    /// Take the current snapshot; never fails, fields degrade to unknown.
    pub fn collect() -> Self {
        Self {
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            shell: std::env::var("SHELL")
                .or_else(|_| std::env::var("ComSpec"))
                .ok(),
        }
    }

    /// Render the snapshot as a prompt block.
    pub fn prompt_block(&self) -> String {
        let mut out = String::from("## Environment\n");
        out.push_str(&format!("OS: {} {}\n", self.os, self.arch));
        out.push_str(&format!("CWD: {}\n", self.cwd.display()));
        if let Some(shell) = &self.shell {
            out.push_str(&format!("Shell: {shell}\n"));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anonymous_profiles_render_nothing() {
        assert!(UserProfile::default().prompt_block().is_empty());
    }

    #[test]
    fn profile_blocks_list_preferences_sorted() {
        let mut profile = UserProfile {
            name: "Ada".to_string(),
            email: None,
            timezone: Some("UTC".to_string()),
            ..UserProfile::default()
        };
        profile.preferences.insert("b".to_string(), "2".to_string());
        profile.preferences.insert("a".to_string(), "1".to_string());
        let block = profile.prompt_block();
        assert!(block.contains("Name: Ada"));
        assert!(block.find("a: 1").unwrap() < block.find("b: 2").unwrap());
    }

    #[test]
    fn environment_collects_without_panicking() {
        let snapshot = EnvironmentSnapshot::collect();
        assert!(!snapshot.os.is_empty());
        assert!(snapshot.prompt_block().contains("## Environment"));
    }
}
