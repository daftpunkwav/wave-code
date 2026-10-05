/*!
 * @file ToolAdapter
 * @description Adapts the shared tool registry (`wavecode_tools::Registry`)
 * to the ToolExecutor seam.
 *
 * Responsibilities:
 * - Look up tools by stable name and execute them with a fixed context.
 * - Translate unknown tools and implementation faults into error results.
 *
 * This module must not depend on: runtime internals beyond its trait seam.
 */

//! [`runtime_runner::ToolExecutor`] implemented over `wavecode-tools`.

use std::path::PathBuf;
use std::sync::Arc;

use runtime_runner::{ToolCall, ToolExecutor, ToolRef, ToolResult};

/// Executes tools from the shared `wavecode_tools` registry with a fixed context.
pub struct ToolAdapter {
    registry: Arc<wavecode_tools::Registry>,
    ctx: wavecode_tools::ToolCtx,
}

impl ToolAdapter {
    /// Wrap a shared registry with the execution context for all calls.
    pub fn new(registry: Arc<wavecode_tools::Registry>, ctx: wavecode_tools::ToolCtx) -> Self {
        Self { registry, ctx }
    }

    /// Build the credential-free `mcp serve` surface: the full builtin
    /// registry (including `todowrite`) plus an executor over it with a
    /// cwd-only execution context. Registry construction lives here, in
    /// the composition root, so frontends keep depending on this crate
    /// instead of naming the tools crate directly; the serving loop
    /// itself lives in the gateway.
    ///
    /// Trust model (accepted posture, on record): a stdio MCP client is
    /// the local process that spawned this server — the operator's own
    /// choice of agent — so `tools/call` runs **without an approval gate**
    /// and without the session policy layer (there is no human on the
    /// server side of the stdio pipe to answer a prompt, and MCP places
    /// tool-approval responsibility on the client). The configured `wave`
    /// denylist is likewise a session-policy artifact and is not consulted
    /// here. What is carried over from session assembly is credential
    /// hygiene: the provider `env_key` names (`serve_deny_env`) are
    /// stripped from tool child environments, so a secret never rides into
    /// a shell spawned through MCP.
    ///
    /// Session-surface degradation (accepted scope, on record): the shell
    /// tool here holds a serve-lifetime spill store (shell and `spill`
    /// share one instance, so spilling and reading back work within the
    /// server process) but **no job service** — a timed-out shell command
    /// kills its process tree instead of promoting to a background job —
    /// and `todowrite` state is process-local, never a session's store.
    /// Both are inherent to a stateless serve surface with no session
    /// behind it; revisiting means serving a real session, not patching
    /// this constructor.
    pub fn mcp_serve_tools(cwd: PathBuf) -> (Arc<wavecode_tools::Registry>, Self) {
        let (registry, _todos) = wavecode_tools::Registry::builtin_with_todos();
        let registry = Arc::new(registry);
        let executor = Self::new(
            registry.clone(),
            wavecode_tools::ToolCtx {
                cwd,
                deny_env: serve_deny_env(),
            },
        );
        (registry, executor)
    }
}

/// Secret env names hidden from tools served over `mcp serve`.
///
/// `mcp serve` needs no model credentials, but the process environment it
/// inherits may still carry provider keys. Session assembly strips the
/// provider's `env_key` from tool children; this surface loads config
/// best-effort for the same names so a configured key never reaches a
/// shell child spawned through MCP. A missing or unreadable config
/// degrades to an empty list — serving never fails on config here, and
/// the shell tool's sensitive-shape fallback still strips common secret
/// names without it.
fn serve_deny_env() -> Vec<String> {
    let Ok(config) = wavecode_config::Config::load() else {
        return Vec::new();
    };
    let mut names: Vec<String> = config
        .model_providers
        .values()
        .filter_map(|provider| provider.env_key.clone())
        .filter(|name| !name.trim().is_empty())
        .collect();
    names.sort();
    names.dedup();
    names
}

#[async_trait::async_trait]
impl ToolExecutor for ToolAdapter {
    async fn execute(&self, call: ToolCall) -> ToolResult {
        let Some(tool) = self.registry.get(&call.name) else {
            return ToolResult {
                call_id: call.call_id,
                content: format!("unknown tool: {}", call.name),
                is_error: true,
            };
        };
        match tool.execute(call.input, &self.ctx).await {
            Ok(output) => ToolResult {
                call_id: call.call_id,
                content: output.content,
                is_error: output.is_error,
            },
            Err(fault) => ToolResult {
                call_id: call.call_id,
                // Implementation faults also ride as error results so the
                // model can self-correct; the shared fault prefix keeps them
                // distinguishable from business failures in transcripts and
                // lets the MCP serve surface map them back to internal errors.
                content: format!("{} {fault}", wavecode_tools::TOOL_FAULT_PREFIX),
                is_error: true,
            },
        }
    }

    /// Attributes come from the tool itself, keeping dispatch truthful.
    fn is_read_only(&self, tool: &str) -> bool {
        self.registry.get(tool).is_some_and(|t| t.is_read_only())
    }

    /// Unknown names stay destructive so dispatch stays serial and safe.
    fn is_destructive(&self, tool: &str) -> bool {
        self.registry.get(tool).is_none_or(|t| t.is_destructive())
    }

    /// Advertise exactly the registered tools.
    fn available_tools(&self) -> Vec<ToolRef> {
        self.registry
            .specs()
            .into_iter()
            .map(|spec| ToolRef {
                name: spec.name.clone(),
                description: spec.description.clone(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wavecode_tools::{Result as ToolResultAlias, Tool, ToolCtx, ToolOutput};

    struct EchoTool;

    #[async_trait::async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo_tool"
        }
        fn description(&self) -> &str {
            "test echo tool"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::Value::Null
        }
        fn is_read_only(&self) -> bool {
            true
        }
        async fn execute(
            &self,
            input: serde_json::Value,
            _ctx: &ToolCtx,
        ) -> ToolResultAlias<ToolOutput> {
            Ok(ToolOutput {
                content: input.to_string(),
                is_error: false,
            })
        }
    }

    struct FaultyTool;

    #[async_trait::async_trait]
    impl Tool for FaultyTool {
        fn name(&self) -> &str {
            "faulty_tool"
        }
        fn description(&self) -> &str {
            "test faulty tool"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::Value::Null
        }
        fn is_read_only(&self) -> bool {
            true
        }
        async fn execute(
            &self,
            _input: serde_json::Value,
            _ctx: &ToolCtx,
        ) -> ToolResultAlias<ToolOutput> {
            Err(std::io::Error::other("disk gone").into())
        }
    }

    fn adapter() -> ToolAdapter {
        let registry = wavecode_tools::Registry::builtin();
        registry.register(Arc::new(EchoTool));
        registry.register(Arc::new(FaultyTool));
        ToolAdapter::new(
            Arc::new(registry),
            ToolCtx {
                cwd: std::path::PathBuf::from("/tmp"),
                deny_env: Vec::new(),
            },
        )
    }

    fn call(name: &str) -> ToolCall {
        ToolCall {
            call_id: "c1".to_string(),
            name: name.to_string(),
            input: serde_json::Value::Null,
        }
    }

    #[tokio::test]
    async fn executes_registered_tools() {
        let out = adapter().execute(call("echo_tool")).await;
        assert!(!out.is_error);
        assert_eq!(out.call_id, "c1");
    }

    #[tokio::test]
    async fn unknown_tools_become_error_results() {
        let out = adapter().execute(call("nope")).await;
        assert!(out.is_error);
        assert!(out.content.contains("unknown tool"));
    }

    #[test]
    fn unknown_tools_stay_destructive() {
        let adapter = adapter();
        // Unknown names take the cautious path (serial dispatch), matching
        // the native and composite executors plus the policy adapter.
        assert!(adapter.is_destructive("nope"));
        assert!(!adapter.is_read_only("nope"));
    }

    #[tokio::test]
    async fn implementation_faults_stay_distinguishable() {
        let out = adapter().execute(call("faulty_tool")).await;
        assert!(out.is_error);
        assert!(out.content.starts_with(wavecode_tools::TOOL_FAULT_PREFIX));
    }

    // serve_deny_env reads the user-level config (home env vars); the
    // env-touching tests run serialized like the config crate's ENV_LOCK.
    static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The serve surface inherits session-assembly credential hygiene: the
    /// configured `env_key` names (all providers, deduplicated) land in
    /// the served executor's deny list.
    #[test]
    fn serve_deny_env_collects_provider_env_keys() {
        let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let wave = dir.path().join(".wavecode");
        std::fs::create_dir_all(&wave).unwrap();
        std::fs::write(
            wave.join("config.toml"),
            r#"
model = "m"
model_provider = "a"

[model_providers.a]
type = "anthropic"
base_url = "https://a.example"
env_key = "A_KEY"

[model_providers.b]
type = "anthropic"
base_url = "https://b.example"
env_key = "A_KEY"
"#,
        )
        .unwrap();
        let saved_user = std::env::var_os("USERPROFILE");
        let saved_home = std::env::var_os("HOME");
        // nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
        unsafe {
            std::env::set_var("USERPROFILE", dir.path());
            std::env::set_var("HOME", dir.path());
        }
        let names = serve_deny_env();
        // nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
        unsafe {
            if let Some(v) = saved_user {
                std::env::set_var("USERPROFILE", v);
            }
            if let Some(v) = saved_home {
                std::env::set_var("HOME", v);
            }
        }
        assert_eq!(names, vec!["A_KEY".to_string()], "dedup across providers");
    }

    /// No config (or no providers): the serve surface stays credential-free
    /// with an empty deny list instead of failing to serve.
    #[test]
    fn serve_deny_env_degrades_to_empty_without_config() {
        let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved_user = std::env::var_os("USERPROFILE");
        let saved_home = std::env::var_os("HOME");
        // nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
        unsafe {
            std::env::set_var("USERPROFILE", tempfile::tempdir().unwrap().path());
            std::env::set_var("HOME", tempfile::tempdir().unwrap().path());
        }
        let names = serve_deny_env();
        // nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
        unsafe {
            if let Some(v) = saved_user {
                std::env::set_var("USERPROFILE", v);
            }
            if let Some(v) = saved_home {
                std::env::set_var("HOME", v);
            }
        }
        assert!(names.is_empty());
    }
}
