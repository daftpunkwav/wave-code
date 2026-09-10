/*!
 * @file LayeredConfig
 * @description Layered key/value configuration with feature flags.
 *
 * Responsibilities:
 * - Resolve keys across defaults, file, and environment layers.
 * - Parse typed values with explicit errors.
 * - Gate behaviour behind named feature flags.
 *
 * This module must not depend on: any other workspace crate. File and
 * environment loading stay with the composition root; this crate merges
 * already-loaded maps.
 */

//! Configuration as data merging: precedence without side effects.

use std::collections::HashMap;

/// Layered string maps with env-over-file-over-defaults precedence.
#[derive(Debug, Clone, Default)]
pub struct LayeredConfig {
    defaults: HashMap<String, String>,
    file: HashMap<String, String>,
    env: HashMap<String, String>,
}

impl LayeredConfig {
    /// Create empty layers.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set one default value (lowest precedence).
    pub fn set_default(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.defaults.insert(key.into(), value.into());
    }

    /// Set one file value (middle precedence).
    pub fn set_file(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.file.insert(key.into(), value.into());
    }

    /// Set one environment override (highest precedence).
    pub fn set_env(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.env.insert(key.into(), value.into());
    }

    /// Resolve one key following layer precedence.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.env
            .get(key)
            .or_else(|| self.file.get(key))
            .or_else(|| self.defaults.get(key))
            .map(String::as_str)
    }

    /// Resolve and parse a boolean (`true`/`false` only).
    pub fn get_bool(&self, key: &str) -> Result<Option<bool>, ConfigError> {
        match self.get(key) {
            None => Ok(None),
            Some("true") => Ok(Some(true)),
            Some("false") => Ok(Some(false)),
            Some(other) => Err(ConfigError::InvalidBool {
                key: key.to_string(),
                value: other.to_string(),
            }),
        }
    }

    /// Resolve and parse an unsigned integer.
    pub fn get_u64(&self, key: &str) -> Result<Option<u64>, ConfigError> {
        match self.get(key) {
            None => Ok(None),
            Some(value) => value
                .parse::<u64>()
                .map(Some)
                .map_err(|_| ConfigError::InvalidInt {
                    key: key.to_string(),
                    value: value.to_string(),
                }),
        }
    }
}

/// Configuration errors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// A boolean value is not `true` or `false`.
    #[error("invalid boolean for {key}: {value:?}")]
    InvalidBool {
        /// Offending key.
        key: String,
        /// Offending value.
        value: String,
    },
    /// An integer value does not parse as u64.
    #[error("invalid integer for {key}: {value:?}")]
    InvalidInt {
        /// Offending key.
        key: String,
        /// Offending value.
        value: String,
    },
}

/// Named feature flags with explicit defaults.
#[derive(Debug, Clone, Default)]
pub struct FlagSet {
    flags: HashMap<String, bool>,
}

impl FlagSet {
    /// Create an empty set (every flag defaults to disabled).
    pub fn new() -> Self {
        Self::default()
    }

    /// Enable one flag.
    pub fn enable(&mut self, name: impl Into<String>) {
        self.flags.insert(name.into(), true);
    }

    /// Disable one flag.
    pub fn disable(&mut self, name: impl Into<String>) {
        self.flags.insert(name.into(), false);
    }

    /// True only when explicitly enabled; unknown flags are disabled.
    pub fn is_enabled(&self, name: &str) -> bool {
        self.flags.get(name).copied().unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precedence_is_env_over_file_over_defaults() {
        let mut config = LayeredConfig::new();
        config.set_default("a", "default");
        config.set_default("b", "default");
        config.set_default("c", "default");
        config.set_file("b", "file");
        config.set_file("c", "file");
        config.set_env("c", "env");
        assert_eq!(config.get("a"), Some("default"));
        assert_eq!(config.get("b"), Some("file"));
        assert_eq!(config.get("c"), Some("env"));
        assert_eq!(config.get("missing"), None);
    }

    #[test]
    fn typed_getters_reject_garbage_explicitly() {
        let mut config = LayeredConfig::new();
        config.set_default("flag", "yes");
        config.set_default("num", "12x");
        assert!(matches!(
            config.get_bool("flag"),
            Err(ConfigError::InvalidBool { .. })
        ));
        assert!(matches!(
            config.get_u64("num"),
            Err(ConfigError::InvalidInt { .. })
        ));
        assert_eq!(config.get_bool("missing").unwrap(), None);
    }

    #[test]
    fn unknown_flags_stay_disabled() {
        let mut flags = FlagSet::new();
        flags.enable("mcp");
        assert!(flags.is_enabled("mcp"));
        assert!(!flags.is_enabled("browser"));
        flags.disable("mcp");
        assert!(!flags.is_enabled("mcp"));
    }
}
