/*!
 * @file ArtifactStore
 * @description Versioned registry of run-produced artifacts.
 *
 * Unwired by intent: zero dependents — not reachable from the
 * `wavecode` binary. Kept as a deliberate seed; see the "Wiring
 * status" section of docs/architecture.md before citing or wiring.
 *
 * Reserved versioned registry of run-produced artifacts; the product's
 * durable data currently rides `state-persistence` / `state-store`.
 * Responsibilities:
 * - Publish artifacts under stable names with auto versions.
 * - Resolve the latest version of a name.
 * - List every stored artifact for inspection.
 *
 * This module must not depend on: any other workspace crate. Payloads
 * stay with producers; the store tracks identity, kind, and integrity.
 */

//! Artifacts: files, code, images, and reports a run produces.
//!
//! Versions increment per name, so overwriting never destroys history;
//! consumers pin either a versioned id or the floating latest pointer.

/// Functional kind of one artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactKind {
    /// General file output.
    File,
    /// Generated or edited code.
    Code,
    /// Generated image.
    Image,
    /// Evaluation or status report.
    Report,
    /// Anything else.
    Other,
}

/// One stored artifact version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    /// Stable identifier `<name>@<version>`.
    pub id: String,
    /// Artifact name shared across versions.
    pub name: String,
    /// Kind of payload.
    pub kind: ArtifactKind,
    /// Version number starting at 1 per name.
    pub version: u32,
    /// Integrity digest supplied by the producer (opaque format).
    pub digest: String,
}

/// Versioned artifact registry.
#[derive(Debug, Default)]
pub struct ArtifactStore {
    artifacts: Vec<Artifact>,
}

impl ArtifactStore {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish one artifact, auto-versioning repeat names.
    pub fn publish(
        &mut self,
        name: impl Into<String>,
        kind: ArtifactKind,
        digest: impl Into<String>,
    ) -> &Artifact {
        let name = name.into();
        let version = self
            .artifacts
            .iter()
            .filter(|a| a.name == name)
            .map(|a| a.version)
            .max()
            .unwrap_or(0)
            + 1;
        let id = format!("{name}@{version}");
        self.artifacts.push(Artifact {
            id,
            name,
            kind,
            version,
            digest: digest.into(),
        });
        self.artifacts.last().expect("just pushed")
    }

    /// Fetch one artifact by versioned id.
    pub fn get(&self, id: &str) -> Option<&Artifact> {
        self.artifacts.iter().find(|a| a.id == id)
    }

    /// Fetch the newest version of a name.
    pub fn latest(&self, name: &str) -> Option<&Artifact> {
        self.artifacts
            .iter()
            .filter(|a| a.name == name)
            .max_by_key(|a| a.version)
    }

    /// Every stored artifact in publish order.
    pub fn list(&self) -> &[Artifact] {
        &self.artifacts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeat_names_version_up_and_latest_resolves() {
        let mut store = ArtifactStore::new();
        store.publish("plan.md", ArtifactKind::Report, "d1");
        store.publish("plan.md", ArtifactKind::Report, "d2");
        assert_eq!(store.latest("plan.md").unwrap().version, 2);
        assert_eq!(store.latest("plan.md").unwrap().digest, "d2");
        assert!(store.get("plan.md@1").is_some());
        assert_eq!(store.list().len(), 2);
    }

    #[test]
    fn unknown_names_resolve_to_none() {
        let store = ArtifactStore::new();
        assert!(store.latest("missing").is_none());
    }
}
