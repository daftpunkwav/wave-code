/*!
 * @file SecretsVault
 * @description Secret storage with redaction for logs and transcripts.
 *
 * Responsibilities:
 * - Hold named secret values behind explicit lookups.
 * - Load values from process environment at composition time.
 * - Redact every known value out of arbitrary text.
 *
 * This module must not depend on: any other workspace crate. Environment
 * reads happen only in the explicit `from_env` constructor, never during
 * recording or redaction.
 */

//! Secrets: explicit handles, never ambient access.
//!
//! Redaction is best-effort hygiene for logs, not a security boundary:
//! values shorter than any real credential still redact, and unknown
//! values cannot be masked by definition.

/// Redaction placeholder replacing secret values.
pub const REDACTED: &str = "***";

/// Named secret store.
#[derive(Debug, Clone, Default)]
pub struct SecretsStore {
    values: std::collections::HashMap<String, String>,
}

impl SecretsStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Load named variables from the process environment now.
    ///
    /// Missing variables are skipped silently so optional credentials do
    /// not fail composition; required ones are validated by the caller.
    pub fn from_env(names: &[&str]) -> Self {
        let mut store = Self::new();
        for name in names {
            if let Ok(value) = std::env::var(name)
                && !value.is_empty()
            {
                store.values.insert(name.to_string(), value);
            }
        }
        store
    }

    /// Insert one secret explicitly (tests, rotations, vault bridges).
    pub fn insert(&mut self, name: impl Into<String>, value: impl Into<String>) {
        let value = value.into();
        if !value.is_empty() {
            self.values.insert(name.into(), value);
        }
    }

    /// Fetch one secret value by name.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    /// Names currently held, for startup diagnostics (never the values).
    pub fn names(&self) -> Vec<&str> {
        self.values.keys().map(String::as_str).collect()
    }

    /// Replace every known value in `text` with the placeholder.
    pub fn redact(&self, text: &str) -> String {
        let mut out = text.to_string();
        let mut values: Vec<&str> = self.values.values().map(String::as_str).collect();
        // Longest first so overlapping values mask fully.
        values.sort_by_key(|v| std::cmp::Reverse(v.len()));
        for value in values {
            out = out.replace(value, REDACTED);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_masks_all_known_values() {
        let mut store = SecretsStore::new();
        store.insert("API_KEY", "sk-secret-1");
        store.insert("TOKEN", "tok");
        let clean = store.redact("key=sk-secret-1 tok and tok again");
        assert!(!clean.contains("sk-secret-1"));
        assert!(!clean.contains("tok"));
        assert!(clean.contains(REDACTED));
    }

    #[test]
    fn empty_values_never_mask_everything() {
        let mut store = SecretsStore::new();
        store.insert("EMPTY", "");
        assert!(store.get("EMPTY").is_none());
        assert_eq!(store.redact("nothing to hide"), "nothing to hide");
    }

    #[test]
    fn missing_names_stay_missing() {
        let store = SecretsStore::from_env(&["WAVECODE_TEST_DEFINITELY_MISSING"]);
        assert!(store.names().is_empty());
    }
}
