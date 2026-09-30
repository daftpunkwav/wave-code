/*!
 * @file CredentialStore
 * @description Provider-scoped credentials for model access.
 *
 * Unwired by intent: zero dependents — not reachable from the
 * `wavecode` binary. Kept as a deliberate seed; see the "Wiring
 * status" section of docs/architecture.md before citing or wiring.
 *
 * Reserved for credential setups beyond `wavecode-config`'s `env_key`
 * path, which is what sessions use today.
 * Responsibilities:
 * - Hold one credential per provider behind explicit lookups.
 * - Load values from the process environment at composition time.
 * - Never expose secret material through Debug formatting.
 *
 * This module must not depend on: any other workspace crate. Durable
 * storage (OS keyrings, vaults) stays out of scope; bridges push values
 * in through `insert` or the explicit `from_env` constructor. Transcript
 * hygiene (masking values out of logs) belongs to the secrets store,
 * which callers feed via `Credential::expose`.
 */

//! Credentials as explicit values, never ambient access.
//!
//! Lookup is always scoped by provider name, so two providers can never
//! share material by accident. The raw secret leaves this crate only
//! through [`Credential::expose`], whose name marks every call site as
//! handling sensitive data.

use std::collections::HashMap;
use std::fmt;

/// Placeholder shown wherever a credential would otherwise print.
pub const REDACTED: &str = "***";

/// Authentication scheme of one credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    /// Key sent as-is (or per provider convention) with each request.
    ApiKey,
    /// Token sent as an HTTP bearer token.
    Bearer,
}

/// One provider credential.
///
/// `Debug` prints only the scheme: secret material never reaches logs
/// through this type, even under `{:#?}` dumps of parent structs.
#[derive(Clone, PartialEq, Eq)]
pub struct Credential {
    scheme: Scheme,
    material: String,
}

impl Credential {
    /// Build an API-key credential; empty values are rejected so a
    /// misconfigured environment fails at composition, not at midnight.
    pub fn api_key(value: impl Into<String>) -> Result<Self, AuthError> {
        Self::of(Scheme::ApiKey, value.into(), "api-key")
    }

    /// Build a bearer-token credential; empty values are rejected.
    pub fn bearer(value: impl Into<String>) -> Result<Self, AuthError> {
        Self::of(Scheme::Bearer, value.into(), "bearer")
    }

    /// Authentication scheme of this credential.
    pub fn scheme(&self) -> Scheme {
        self.scheme
    }

    /// Raw secret material for request signing.
    ///
    /// Handle with care: never log the return value. Feed it to the
    /// secrets store when its containing text may reach transcripts.
    pub fn expose(&self) -> &str {
        &self.material
    }

    fn of(scheme: Scheme, value: String, what: &str) -> Result<Self, AuthError> {
        if value.is_empty() {
            return Err(AuthError::Empty(what.to_string()));
        }
        Ok(Self {
            scheme,
            material: value,
        })
    }
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credential")
            .field("scheme", &self.scheme)
            .field("material", &REDACTED)
            .finish()
    }
}

/// Credential store failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// A credential value was empty; carries what was being built.
    Empty(String),
    /// No credential exists for the provider.
    Unknown(String),
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthError::Empty(what) => write!(f, "empty credential value for {what}"),
            AuthError::Unknown(provider) => {
                write!(f, "no credential stored for provider {provider:?}")
            }
        }
    }
}

impl std::error::Error for AuthError {}

/// Provider-keyed credential store.
#[derive(Debug, Clone, Default)]
pub struct AuthStore {
    entries: HashMap<String, Credential>,
}

impl AuthStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Load credentials from the process environment now.
    ///
    /// Each pair names a provider and the variable holding its key;
    /// missing or empty variables are skipped so optional providers do
    /// not fail composition. The scheme defaults to API key; bearer
    /// tokens arrive through [`AuthStore::insert`] instead.
    pub fn from_env(pairs: &[(&str, &str)]) -> Self {
        let mut store = Self::new();
        for (provider, var) in pairs {
            if let Ok(value) = std::env::var(var)
                && !value.is_empty()
                && let Ok(credential) = Credential::api_key(value)
            {
                store.entries.insert(provider.to_string(), credential);
            }
        }
        store
    }

    /// Store (or rotate) one provider credential, replacing any prior one.
    pub fn insert(&mut self, provider: impl Into<String>, credential: Credential) {
        self.entries.insert(provider.into(), credential);
    }

    /// Drop one provider credential, returning it when present.
    pub fn remove(&mut self, provider: &str) -> Option<Credential> {
        self.entries.remove(provider)
    }

    /// Fetch one provider credential by name.
    pub fn get(&self, provider: &str) -> Option<&Credential> {
        self.entries.get(provider)
    }

    /// Fetch one provider credential, failing explicitly when absent.
    pub fn get_or(&self, provider: &str) -> Result<&Credential, AuthError> {
        self.entries
            .get(provider)
            .ok_or_else(|| AuthError::Unknown(provider.to_string()))
    }

    /// Provider names currently held, for startup diagnostics.
    pub fn providers(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.entries.keys().map(String::as_str).collect();
        names.sort_unstable();
        names
    }

    /// Number of stored credentials.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when no credential is stored.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_values_rejected_at_construction() {
        assert!(matches!(Credential::api_key(""), Err(AuthError::Empty(_))));
        assert!(matches!(Credential::bearer(""), Err(AuthError::Empty(_))));
        assert!(Credential::api_key("k").unwrap().expose() == "k");
    }

    #[test]
    fn debug_never_leaks_material() {
        let credential = Credential::bearer("super-secret-token").unwrap();
        let dump = format!("{credential:?}");
        assert!(!dump.contains("super-secret-token"));
        assert!(dump.contains(REDACTED));
        assert!(dump.contains("Bearer"));
    }

    #[test]
    fn insert_get_remove_round_trip() {
        let mut store = AuthStore::new();
        assert!(store.is_empty());
        store.insert("acme", Credential::api_key("k1").unwrap());
        store.insert("other", Credential::bearer("t").unwrap());
        assert_eq!(store.len(), 2);
        assert_eq!(store.get("acme").unwrap().scheme(), Scheme::ApiKey);
        // Rotation replaces without growing.
        store.insert("acme", Credential::api_key("k2").unwrap());
        assert_eq!(store.len(), 2);
        assert_eq!(store.get("acme").unwrap().expose(), "k2");
        assert_eq!(store.providers(), vec!["acme", "other"]);
        assert!(store.remove("acme").is_some());
        assert!(store.get("acme").is_none());
    }

    #[test]
    fn missing_providers_fail_explicitly() {
        let store = AuthStore::new();
        assert_eq!(
            store.get_or("ghost").unwrap_err(),
            AuthError::Unknown("ghost".to_string())
        );
        assert_eq!(
            AuthError::Unknown("x".to_string()).to_string(),
            "no credential stored for provider \"x\""
        );
    }

    #[test]
    fn env_loading_skips_missing_without_failing() {
        let store = AuthStore::from_env(&[("ghost", "WAVECODE_TEST_DEFINITELY_MISSING")]);
        assert!(store.is_empty());
    }
}
