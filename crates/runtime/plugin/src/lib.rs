/*! @file plugin
 * @description Minimal runtime plugin system: service injection, middleware hooks, in-session lifecycle.
 *
 * Responsibilities:
 * - Define the Plugin trait and the AnyService type-erased service contract.
 * - Start plugins in manifest dependency order; stop them in reverse order.
 * - Discover runtime.toml manifests with warn-and-skip degradation.
 *
 * This module must not depend on: runtime internals, capabilities, frontends, wire.
 */

//! Minimal runtime plugin system (service injection + middleware + lifecycle).
//!
//! A [`Plugin`] contributes type-erased [`AnyService`] objects into a shared
//! [`ServiceMap`] (keyed by [`TypeId`]) and observes load/unload lifecycle.
//! The [`Registry`] starts plugins in manifest dependency order and stops
//! them in reverse; duplicate names, missing dependencies, and dependency
//! cycles are explicit [`PluginError`] values, never panics.
//!
//! Middleware ordering contract (see `wavecode-hooks`): config hooks execute
//! first, plugin hooks run after, in registration order.
//!
//! Manifests (`<home>/.wavecode/plugins/*/runtime.toml`) carry only
//! `{name, version, depends?}` identity: [`discover`] warns-and-skips invalid
//! plugins and never fails assembly. Hot removal is [`Registry::unload`]
//! (no file watching).

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Registry and loader failures as explicit business errors (never panics).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PluginError {
    /// Two plugins share one name (or a name is blank).
    #[error("duplicate plugin name: {0}")]
    Duplicate(String),
    /// A plugin depends on a name that was never registered.
    #[error("plugin {plugin} depends on missing plugin {dep}")]
    MissingDep { plugin: String, dep: String },
    /// Dependency edges form a cycle (path ends where it starts).
    #[error("cyclic plugin dependencies: {cycle}", cycle = .0.join(" -> "))]
    Cycle(Vec<String>),
}

/// Type-erased injectable service: one explicit implementation per
/// concrete service struct.
///
/// [`ServiceMap`] keys each service by the [`TypeId`] of its concrete type.
///
/// Never add a blanket `impl<T> AnyService for T`: it would also cover
/// `Arc<dyn AnyService>` itself, and method probing then resolves
/// `arc.service_id()` / `arc.as_any()` to that Arc-level impl (no deref)
/// instead of the vtable (after deref) whenever this trait is in scope —
/// silently keying every service by one shared `TypeId` and breaking all
/// typed retrieval.
pub trait AnyService: Send + Sync + 'static {
    /// Concrete-type key for [`ServiceMap`] (defaults to `TypeId::of::<Self>`).
    fn service_id(&self) -> TypeId
    where
        Self: 'static,
    {
        TypeId::of::<Self>()
    }

    /// Downcasting view for typed retrieval.
    fn as_any(&self) -> &dyn Any;
}

/// Shared service container keyed by concrete-type [`TypeId`].
#[derive(Default)]
pub struct ServiceMap {
    inner: HashMap<TypeId, Arc<dyn AnyService>>,
}

impl ServiceMap {
    /// Create an empty container.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a typed service, keyed by `TypeId::of::<T>()`.
    pub fn insert_typed<T: AnyService>(&mut self, service: Arc<T>) {
        self.inner
            .insert(TypeId::of::<T>(), service as Arc<dyn AnyService>);
    }

    /// Insert an already-erased service, keyed by its own `service_id()`.
    pub fn insert_erased(&mut self, service: Arc<dyn AnyService>) {
        let id = service.service_id();
        eprintln!("DBG insert id={id:?} dyn-obj={:?}", TypeId::of::<dyn AnyService>());
        self.inner.insert(id, service);
    }

    /// Retrieve a service by concrete type.
    pub fn get<T: AnyService>(&self) -> Option<&T> {
        self.inner
            .get(&TypeId::of::<T>())
            .and_then(|service| service.as_any().downcast_ref::<T>())
    }

    /// True when a service of this concrete type is present.
    pub fn contains<T: AnyService>(&self) -> bool {
        self.inner.contains_key(&TypeId::of::<T>())
    }

    /// Remove one entry by [`TypeId`]; true when something was removed.
    pub fn remove(&mut self, id: TypeId) -> bool {
        self.inner.remove(&id).is_some()
    }

    /// Number of registered services.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// True when no services are registered.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

/// One runtime plugin: identity plus injectable services and lifecycle hooks.
///
/// `on_load` runs at [`Registry::start`] in dependency order; `on_unload`
/// runs at [`Registry::unload`] (hot removal). Both default to no-ops.
pub trait Plugin: Send + Sync {
    /// Stable plugin name (unique within one [`Registry`]).
    fn name(&self) -> &str;

    /// Opaque version string for display.
    fn version(&self) -> &str;

    /// Services contributed to the shared [`ServiceMap`] on start.
    fn services(&self) -> Vec<Arc<dyn AnyService>> {
        Vec::new()
    }

    /// Load hook (runs in dependency order at start).
    fn on_load(&self) {}

    /// Unload hook (runs on hot removal).
    fn on_unload(&self) {}
}

/// One registered plugin plus its manifest dependency names.
struct Entry {
    plugin: Arc<dyn Plugin>,
    depends: Vec<String>,
}

/// Ordered plugin registry with service injection and lifecycle.
///
/// Register every plugin first, then [`Registry::start`]: start resolves the
/// dependency order, invokes `on_load` in that order, and injects services.
/// [`Registry::unload`] invokes `on_unload`, withdraws that plugin's
/// services, and drops it (hot removal, no file watching).
#[derive(Default)]
pub struct Registry {
    entries: HashMap<String, Entry>,
    order: Vec<String>,
    services: ServiceMap,
    owners: HashMap<String, Vec<TypeId>>,
    started: bool,
}

impl Registry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register one plugin; duplicates (or blank names) are rejected.
    pub fn register(
        &mut self,
        plugin: Arc<dyn Plugin>,
        depends: Vec<String>,
    ) -> Result<(), PluginError> {
        let name = plugin.name().to_owned();
        if name.trim().is_empty() {
            return Err(PluginError::Duplicate(name));
        }
        if self.entries.contains_key(&name) {
            return Err(PluginError::Duplicate(name));
        }
        self.entries.insert(name, Entry { plugin, depends });
        Ok(())
    }

    /// Start all registered plugins in dependency order.
    ///
    /// Missing dependencies and cycles fail here as [`PluginError`]; nothing
    /// is partially started on failure. Repeated calls are no-ops.
    pub fn start(&mut self) -> Result<(), PluginError> {
        if self.started {
            return Ok(());
        }
        let order = self.resolve_order()?;
        for name in &order {
            let entry = &self.entries[name.as_str()];
            entry.plugin.on_load();
            let mut owned = Vec::new();
            for service in entry.plugin.services() {
                owned.push(service.service_id());
                self.services.insert_erased(service);
            }
            self.owners.insert(name.clone(), owned);
        }
        self.order = order;
        self.started = true;
        Ok(())
    }

    /// Hot-remove one plugin: runs `on_unload`, withdraws its services, and
    /// drops it. True when the plugin existed; false is not an error.
    pub fn unload(&mut self, name: &str) -> bool {
        let Some(entry) = self.entries.remove(name) else {
            return false;
        };
        entry.plugin.on_unload();
        if let Some(ids) = self.owners.remove(name) {
            for id in ids {
                self.services.remove(id);
            }
        }
        self.order.retain(|member| member != name);
        if self.entries.is_empty() {
            self.started = false;
            self.order.clear();
        }
        true
    }

    /// True when a plugin with this name is registered.
    pub fn contains(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }

    /// Number of registered plugins.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when no plugins are registered.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Start order from the last successful [`Registry::start`].
    pub fn load_order(&self) -> &[String] {
        &self.order
    }

    /// Shared injected services.
    pub fn services(&self) -> &ServiceMap {
        &self.services
    }

    /// Topological start order (dependencies first, names tie-broken
    /// alphabetically for determinism).
    fn resolve_order(&self) -> Result<Vec<String>, PluginError> {
        let mut order = Vec::new();
        let mut state: HashMap<String, u8> = HashMap::new();
        let mut stack: Vec<String> = Vec::new();
        let mut names: Vec<&String> = self.entries.keys().collect();
        names.sort();
        for name in names {
            visit(name.as_str(), &self.entries, &mut state, &mut stack, &mut order)?;
        }
        Ok(order)
    }
}

/// Depth-first ordering visit: missing deps and cycles are business errors.
fn visit(
    name: &str,
    entries: &HashMap<String, Entry>,
    state: &mut HashMap<String, u8>,
    stack: &mut Vec<String>,
    order: &mut Vec<String>,
) -> Result<(), PluginError> {
    match state.get(name) {
        Some(2) => return Ok(()),
        Some(1) => {
            let mut cycle: Vec<String> = stack
                .iter()
                .skip_while(|member| member.as_str() != name)
                .cloned()
                .collect();
            cycle.push(name.to_owned());
            return Err(PluginError::Cycle(cycle));
        }
        _ => {}
    }
    state.insert(name.to_owned(), 1);
    stack.push(name.to_owned());
    // Clone the edge list so the entries borrow ends before recursion.
    let depends: Vec<String> = entries
        .get(name)
        .map(|entry| entry.depends.clone())
        .unwrap_or_default();
    for dep in &depends {
        if !entries.contains_key(dep.as_str()) {
            return Err(PluginError::MissingDep {
                plugin: name.to_owned(),
                dep: dep.clone(),
            });
        }
        visit(dep.as_str(), entries, state, stack, order)?;
    }
    stack.pop();
    state.insert(name.to_owned(), 2);
    order.push(name.to_owned());
    Ok(())
}

/// Raw `runtime.toml` manifest shape (keys are the manifest keys).
#[derive(Debug, Clone, serde::Deserialize)]
struct ManifestFile {
    /// Plugin name (non-empty).
    name: String,
    /// Opaque version string (non-empty).
    version: String,
    /// Names that must start before this plugin (default: none).
    #[serde(default)]
    depends: Vec<String>,
}

/// One discovered manifest plus the directory that holds it.
#[derive(Debug, Clone)]
pub struct DiscoveredPlugin {
    /// Validated plugin name.
    pub name: String,
    /// Validated version string.
    pub version: String,
    /// Dependency names from the manifest (default: empty).
    pub depends: Vec<String>,
    /// Directory containing `runtime.toml`.
    pub dir: PathBuf,
}

/// Manifest-only plugin: identity with no services (TOML carries no code).
struct ManifestPlugin {
    name: String,
    version: String,
}

impl Plugin for ManifestPlugin {
    fn name(&self) -> &str {
        &self.name
    }

    fn version(&self) -> &str {
        &self.version
    }
}

/// Discover manifests under `<home>/.wavecode/plugins/*/runtime.toml`.
///
/// Invalid plugins warn-and-skip into `warnings` and never fail assembly:
/// unreadable directories, unparsable TOML, blank names/versions, and blank
/// dependency entries. Directories without a `runtime.toml` are silently
/// skipped (they belong to other plugin kinds).
pub fn discover(home: Option<&Path>, warnings: &mut Vec<String>) -> Vec<DiscoveredPlugin> {
    let Some(home) = home else {
        return Vec::new();
    };
    let root = home.join(".wavecode").join("plugins");
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    let mut dirs: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        if path.is_dir() {
            dirs.push(path);
        }
    }
    dirs.sort();
    let mut found = Vec::new();
    for dir in dirs {
        let manifest_path = dir.join("runtime.toml");
        if !manifest_path.is_file() {
            continue;
        }
        let label = dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| dir.display().to_string());
        let text = match std::fs::read_to_string(&manifest_path) {
            Ok(text) => text,
            Err(e) => {
                warnings.push(format!("runtime plugin {label:?} skipped: {e}"));
                continue;
            }
        };
        let file: ManifestFile = match toml::from_str(&text) {
            Ok(file) => file,
            Err(e) => {
                warnings.push(format!("runtime plugin {label:?} skipped: invalid manifest: {e}"));
                continue;
            }
        };
        if file.name.trim().is_empty() || file.version.trim().is_empty() {
            warnings.push(format!(
                "runtime plugin {label:?} skipped: name and version must be non-empty"
            ));
            continue;
        }
        if file.depends.iter().any(|dep| dep.trim().is_empty()) {
            warnings.push(format!(
                "runtime plugin {label:?} skipped: dependency names must be non-empty"
            ));
            continue;
        }
        found.push(DiscoveredPlugin {
            name: file.name,
            version: file.version,
            depends: file.depends,
            dir,
        });
    }
    found
}

/// Discover manifests and start them; graph problems degrade to warnings.
///
/// This never fails: discovery skips invalid plugins, duplicate names skip
/// one copy, and a failed [`Registry::start`] keeps the registered entries
/// while recording the reason. Callers keep the returned [`Registry`] alive
/// for the session so [`Registry::unload`] stays available.
pub fn load_and_start(home: Option<&Path>, warnings: &mut Vec<String>) -> Registry {
    let mut registry = Registry::new();
    for discovered in discover(home, warnings) {
        let plugin: Arc<dyn Plugin> = Arc::new(ManifestPlugin {
            name: discovered.name.clone(),
            version: discovered.version.clone(),
        });
        if let Err(e) = registry.register(plugin, discovered.depends.clone()) {
            warnings.push(format!("runtime plugin {:?} skipped: {e}", discovered.name));
        }
    }
    if let Err(e) = registry.start() {
        warnings.push(format!("runtime plugins failed to start: {e}"));
    }
    registry
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Probe {
        name: String,
        log: Arc<Mutex<Vec<String>>>,
        offered: Vec<Arc<dyn AnyService>>,
    }

    impl Probe {
        fn named(name: &str, log: &Arc<Mutex<Vec<String>>>) -> Self {
            Self {
                name: name.to_owned(),
                log: log.clone(),
                offered: Vec::new(),
            }
        }
    }

    impl Plugin for Probe {
        fn name(&self) -> &str {
            &self.name
        }

        fn version(&self) -> &str {
            "1.0.0"
        }

        fn services(&self) -> Vec<Arc<dyn AnyService>> {
            self.offered.clone()
        }

        fn on_load(&self) {
            self.log
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(format!("load:{}", self.name));
        }

        fn on_unload(&self) {
            self.log
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(format!("unload:{}", self.name));
        }
    }

    #[derive(Debug, PartialEq)]
    struct ServiceA(u32);

    impl AnyService for ServiceA {
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    fn logged(log: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
        log.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn register(registry: &mut Registry, plugin: Probe, depends: &[&str]) {
        registry
            .register(
                Arc::new(plugin),
                depends.iter().map(|dep| dep.to_string()).collect(),
            )
            .unwrap();
    }

    #[test]
    fn starts_in_dependency_order() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut registry = Registry::new();
        register(&mut registry, Probe::named("c", &log), &["b"]);
        register(&mut registry, Probe::named("b", &log), &["a"]);
        register(&mut registry, Probe::named("a", &log), &[]);
        registry.start().unwrap();
        assert_eq!(registry.load_order(), &["a", "b", "c"]);
        assert_eq!(logged(&log), vec!["load:a", "load:b", "load:c"]);
    }

    #[test]
    fn duplicates_rejected_without_panics() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut registry = Registry::new();
        register(&mut registry, Probe::named("a", &log), &[]);
        let err = registry
            .register(Arc::new(Probe::named("a", &log)), Vec::new())
            .unwrap_err();
        assert_eq!(err, PluginError::Duplicate("a".to_owned()));
    }

    #[test]
    fn missing_dependencies_fail_start_explicitly() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut registry = Registry::new();
        register(&mut registry, Probe::named("a", &log), &["ghost"]);
        let err = registry.start().unwrap_err();
        assert_eq!(
            err,
            PluginError::MissingDep {
                plugin: "a".to_owned(),
                dep: "ghost".to_owned()
            }
        );
        assert!(logged(&log).is_empty());
    }

    #[test]
    fn cycles_fail_start_explicitly() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut registry = Registry::new();
        register(&mut registry, Probe::named("a", &log), &["b"]);
        register(&mut registry, Probe::named("b", &log), &["a"]);
        let err = registry.start().unwrap_err();
        assert!(matches!(err, PluginError::Cycle(_)), "{err:?}");
        assert!(logged(&log).is_empty());
    }

    /// Services inject by concrete type and withdraw on unload.
    ///
    /// This also guards vtable dispatch: it runs with this trait in scope,
    /// so a blanket `impl<T> AnyService for T` (forbidden, see the trait
    /// docs) would key by the shared `Arc<dyn AnyService>` id and fail here.
    #[test]
    fn services_inject_by_type_and_withdraw_on_unload() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut offering = Probe::named("a", &log);
        offering.offered.push(Arc::new(ServiceA(7)));
        let mut registry = Registry::new();
        register(&mut registry, offering, &[]);
        registry.start().unwrap();
        assert_eq!(registry.services().get::<ServiceA>(), Some(&ServiceA(7)));
        assert!(registry.unload("a"));
        assert!(!registry.services().contains::<ServiceA>());
        assert!(!registry.unload("a"));
        assert_eq!(logged(&log), vec!["load:a", "unload:a"]);
    }

    #[test]
    fn unload_keeps_remaining_start_order() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut registry = Registry::new();
        register(&mut registry, Probe::named("b", &log), &["a"]);
        register(&mut registry, Probe::named("a", &log), &[]);
        registry.start().unwrap();
        assert!(registry.unload("b"));
        assert_eq!(registry.load_order(), &["a"]);
        assert!(registry.contains("a"));
    }

    #[test]
    fn loader_skips_bad_manifests_in_tempdir() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join(".wavecode").join("plugins");
        let write = |dir: &str, file: Option<&str>| {
            let dir = root.join(dir);
            std::fs::create_dir_all(&dir).unwrap();
            if let Some(text) = file {
                std::fs::write(dir.join("runtime.toml"), text).unwrap();
            }
        };
        write("good", Some("name = \"good\"\nversion = \"1.0.0\"\n"));
        write("broken", Some("name = \nversion = "));
        write("blank", Some("name = \"\"\nversion = \"1.0.0\"\n"));
        write("other", None);
        let mut warnings = Vec::new();
        let found = discover(Some(home.path()), &mut warnings);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "good");
        assert!(warnings.len() >= 2, "{warnings:?}");
        assert!(warnings.iter().all(|w| w.contains("skipped")));
    }

    #[test]
    fn load_and_start_never_fails_assembly() {
        let mut warnings = Vec::new();
        let registry = load_and_start(None, &mut warnings);
        assert!(registry.is_empty());
        assert!(warnings.is_empty());
    }
}
