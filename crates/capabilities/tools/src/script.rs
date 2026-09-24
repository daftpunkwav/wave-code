/*! @file Script
 * @description Agent-executable Python and Node script tools.
 *
 * Responsibilities:
 * - Discover `python3`/`python`/`py` and `node` interpreters on PATH
 * - Execute inline scripts with extra args, cwd confinement, and timeout
 * - Scrub sensitive env vars and truncate output via the shell helpers
 *
 * This module must not depend on: UI-layer components, network access.
 */

//! `python` / `node` tools: execute inline scripts with a discovered
//! interpreter. Scripts run non-interactively (stdin closed) with
//! `ctx.cwd` as cwd; no path parameters are accepted, so confinement is
//! structural rather than validated. Failure semantics mirror `shell_tool`:
//! business failures (missing interpreter, bad params, timeout, nonzero exit)
//! return `Ok(is_error=true)`; `Err` is reserved for implementation faults.

use std::time::Duration;

use serde_json::{Value, json};

use crate::{Result, Tool, ToolCtx, ToolOutput, err_output};

/// Default timeout: 60 s.
const DEFAULT_TIMEOUT_MS: u64 = 60_000;
/// Timeout ceiling: 300 s, values above are clamped.
const MAX_TIMEOUT_MS: u64 = 300_000;

/// Interpreter candidates in discovery order (Windows-aware: `py`
/// launcher last).
fn python_candidates() -> &'static [&'static str] {
    &["python3", "python", "py"]
}

/// Node has a single conventional binary name on every platform.
fn node_candidates() -> &'static [&'static str] {
    &["node"]
}

/// Look up the first candidate present on `PATH` (existence probe only;
/// spawn still resolves via `PATH`, so renames between probe and spawn
/// surface as a business error, not a panic).
fn discover(candidates: &[&str]) -> Option<String> {
    let path_var = std::env::var_os("PATH")?;
    for name in candidates {
        for dir in std::env::split_paths(&path_var) {
            if dir.join(name).is_file() {
                return Some((*name).to_owned());
            }
            // Windows: `py`/`python` may resolve via PATHEXT
            // (`py.exe`, `python.bat`, ...); probe the common suffixes.
            if cfg!(windows) {
                for ext in ["exe", "bat", "cmd", "com"] {
                    if dir.join(format!("{name}.{ext}")).is_file() {
                        return Some((*name).to_owned());
                    }
                }
            }
        }
    }
    None
}

/// Parse `timeout_ms` (default 60 s, clamped to 300 s).
fn resolve_timeout(input: &Value) -> std::result::Result<u64, ToolOutput> {
    match input.get("timeout_ms") {
        None | Some(Value::Null) => Ok(DEFAULT_TIMEOUT_MS),
        Some(v) => match v.as_u64() {
            Some(n) => Ok(n.min(MAX_TIMEOUT_MS)),
            None => Err(err_output(
                "invalid parameter 'timeout_ms' (non-negative integer required)",
            )),
        },
    }
}

/// Parse optional `args` (array of strings).
fn parse_args(input: &Value) -> std::result::Result<Vec<String>, ToolOutput> {
    match input.get("args") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str().map(str::to_owned).ok_or_else(|| {
                    err_output("invalid parameter 'args' (array of strings required)")
                })
            })
            .collect(),
        Some(_) => Err(err_output(
            "invalid parameter 'args' (array of strings required)",
        )),
    }
}

/// Build the interpreter argv: `<flag> <script> <extra...>`.
/// Python uses `-c`, Node uses `-e`; trailing args land in
/// `sys.argv[1:]` / `process.argv.slice(1)` respectively.
fn build_argv(flag: &str, script: &str, extra: &[String]) -> Vec<String> {
    let mut argv = Vec::with_capacity(2 + extra.len());
    argv.push(flag.to_owned());
    argv.push(script.to_owned());
    argv.extend(extra.iter().cloned());
    argv
}

/// Shared spawn path: stdin closed, cwd confined, env scrubbed, timeout
/// kills the process, both output streams truncated via the shell helper.
///
/// Output capture rides the shell tool's capped path (`spawn_and_collect`
/// with [`crate::shell_tool::STREAM_CAPTURE_CAP`]): reads run to EOF so a
/// full pipe cannot deadlock, and buffering stops at the cap so a chatty
/// script cannot grow the process by output-rate x timeout. The visible
/// contract is unchanged (`truncate_output` still cuts to the display cap).
async fn run_script(
    program: &str,
    argv: &[String],
    timeout_ms: u64,
    ctx: &ToolCtx,
) -> Result<ToolOutput> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(argv)
        .current_dir(&ctx.cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    // Same scrubbing as Shell: deny_env list plus sensitive-suffix fallback.
    crate::shell_tool::sanitize_env(&mut cmd, ctx);
    let output = match tokio::time::timeout(
        Duration::from_millis(timeout_ms),
        crate::shell_tool::spawn_and_collect(&mut cmd, crate::shell_tool::STREAM_CAPTURE_CAP),
    )
    .await
    {
        Ok(Ok(output)) => output,
        Err(_) => {
            return Ok(err_output(format!(
                "timeout after {timeout_ms}ms: {program} {}",
                argv.first().map_or("", String::as_str)
            )));
        }
        Ok(Err(e)) => {
            return Ok(err_output(format!("failed to spawn {program}: {e}")));
        }
    };
    let code = output.status.code().unwrap_or(-1);
    let stdout = crate::shell_tool::truncate_output(&output.stdout);
    let stderr = crate::shell_tool::truncate_output(&output.stderr);
    let mut content = format!("exit code: {code}");
    if !stdout.is_empty() {
        content.push_str(&format!("\n--- stdout ---\n{stdout}"));
    }
    if !stderr.is_empty() {
        content.push_str(&format!("\n--- stderr ---\n{stderr}"));
    }
    Ok(ToolOutput {
        content,
        is_error: code != 0,
    })
}

/// Execute a Python script (writes possible: not read-only; destructive
/// approval mirrors Shell, i.e. default non-destructive).
pub struct PythonTool;

#[async_trait::async_trait]
impl Tool for PythonTool {
    fn name(&self) -> &str {
        "python"
    }

    fn description(&self) -> &str {
        "Run an inline Python script in the working directory with a discovered \
         interpreter (python3, then python, then py). Pass extra CLI args via args \
         (visible as sys.argv[1:]). Use timeout_ms to bound execution (default \
         60000 ms, clamped to 300000 ms); on timeout the process is killed. \
         stdout and stderr are captured separately, each truncated at 30KB. \
         Runs non-interactive (stdin is closed) with cwd as working directory."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Python script source to execute"
                },
                "args": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Extra CLI args appended after the script (sys.argv[1:])"
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Timeout in milliseconds (default 60000, clamped to max 300000)"
                }
            },
            "required": ["command"]
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let script = match input.get("command").and_then(Value::as_str) {
            Some(c) => c,
            None => {
                return Ok(err_output(
                    "missing or invalid parameter 'command' (string required)",
                ));
            }
        };
        let extra = match parse_args(&input) {
            Ok(a) => a,
            Err(out) => return Ok(out),
        };
        let timeout_ms = match resolve_timeout(&input) {
            Ok(t) => t,
            Err(out) => return Ok(out),
        };
        let program = match discover(python_candidates()) {
            Some(p) => p,
            None => {
                return Ok(err_output(
                    "no Python interpreter found on PATH (looked for python3, python, py)",
                ));
            }
        };
        let argv = build_argv("-c", script, &extra);
        run_script(&program, &argv, timeout_ms, ctx).await
    }
}

/// Execute a Node.js script (writes possible: not read-only; destructive
/// approval mirrors Shell, i.e. default non-destructive).
pub struct NodeTool;

#[async_trait::async_trait]
impl Tool for NodeTool {
    fn name(&self) -> &str {
        "node"
    }

    fn description(&self) -> &str {
        "Run an inline Node.js script in the working directory with a discovered \
         node interpreter. Pass extra CLI args via args (visible as \
         process.argv.slice(1)). Use timeout_ms to bound execution (default \
         60000 ms, clamped to 300000 ms); on timeout the process is killed. \
         stdout and stderr are captured separately, each truncated at 30KB. \
         Runs non-interactive (stdin is closed) with cwd as working directory."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Node.js script source to execute"
                },
                "args": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Extra CLI args appended after the script (process.argv.slice(1))"
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Timeout in milliseconds (default 60000, clamped to max 300000)"
                }
            },
            "required": ["command"]
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let script = match input.get("command").and_then(Value::as_str) {
            Some(c) => c,
            None => {
                return Ok(err_output(
                    "missing or invalid parameter 'command' (string required)",
                ));
            }
        };
        let extra = match parse_args(&input) {
            Ok(a) => a,
            Err(out) => return Ok(out),
        };
        let timeout_ms = match resolve_timeout(&input) {
            Ok(t) => t,
            Err(out) => return Ok(out),
        };
        let program = match discover(node_candidates()) {
            Some(p) => p,
            None => {
                return Ok(err_output(
                    "no Node interpreter found on PATH (looked for node)",
                ));
            }
        };
        let argv = build_argv("-e", script, &extra);
        run_script(&program, &argv, timeout_ms, ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_defaults_and_clamps() {
        assert_eq!(
            resolve_timeout(&serde_json::json!({})).unwrap(),
            DEFAULT_TIMEOUT_MS
        );
        assert_eq!(
            resolve_timeout(&serde_json::json!({"timeout_ms": null})).unwrap(),
            DEFAULT_TIMEOUT_MS
        );
        assert_eq!(
            resolve_timeout(&serde_json::json!({"timeout_ms": 1000})).unwrap(),
            1000
        );
        // Ceiling clamps instead of rejecting.
        assert_eq!(
            resolve_timeout(&serde_json::json!({"timeout_ms": 999_999_999})).unwrap(),
            MAX_TIMEOUT_MS
        );
        assert!(resolve_timeout(&serde_json::json!({"timeout_ms": -1})).is_err());
        assert!(resolve_timeout(&serde_json::json!({"timeout_ms": "fast"})).is_err());
        assert!(resolve_timeout(&serde_json::json!({"timeout_ms": 1.5})).is_err());
    }

    #[test]
    fn candidates_prefer_python3_then_py_launcher() {
        assert_eq!(python_candidates(), &["python3", "python", "py"]);
        assert_eq!(node_candidates(), &["node"]);
    }

    #[test]
    fn argv_puts_script_before_extra_args() {
        let argv = build_argv("-c", "print('hi')", &["a".to_owned(), "b".to_owned()]);
        assert_eq!(argv, vec!["-c", "print('hi')", "a", "b"]);
        let bare = build_argv("-e", "1+1", &[]);
        assert_eq!(bare, vec!["-e", "1+1"]);
    }

    #[test]
    fn args_parsing_accepts_missing_and_rejects_non_strings() {
        assert!(parse_args(&serde_json::json!({})).unwrap().is_empty());
        assert_eq!(
            parse_args(&serde_json::json!({"args": ["x"]})).unwrap(),
            vec!["x".to_owned()]
        );
        assert!(parse_args(&serde_json::json!({"args": [1]})).is_err());
        assert!(parse_args(&serde_json::json!({"args": "x"})).is_err());
    }

    fn ctx() -> (tempfile::TempDir, ToolCtx) {
        let dir = tempfile::tempdir().unwrap();
        let c = ToolCtx {
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
        };
        (dir, c)
    }

    #[tokio::test]
    async fn python_runs_and_sees_extra_args() {
        if discover(python_candidates()).is_none() {
            eprintln!("skipping: no Python interpreter on PATH");
            return;
        }
        let (_d, c) = ctx();
        let out = PythonTool
            .execute(
                serde_json::json!({"command": "import sys; print(sys.argv[1:])", "args": ["hi"]}),
                &c,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "unexpected failure: {}", out.content);
        assert!(out.content.contains("hi"));
    }

    #[tokio::test]
    async fn python_nonzero_exit_is_error_output() {
        if discover(python_candidates()).is_none() {
            eprintln!("skipping: no Python interpreter on PATH");
            return;
        }
        let (_d, c) = ctx();
        let out = PythonTool
            .execute(serde_json::json!({"command": "raise SystemExit(3)"}), &c)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("exit code: 3"));
    }

    #[tokio::test]
    async fn python_timeout_kills_script() {
        if discover(python_candidates()).is_none() {
            eprintln!("skipping: no Python interpreter on PATH");
            return;
        }
        let (_d, c) = ctx();
        let out = PythonTool
            .execute(
                serde_json::json!({"command": "import time; time.sleep(30)", "timeout_ms": 500}),
                &c,
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.to_lowercase().contains("timeout"));
    }

    #[tokio::test]
    async fn node_runs_when_available() {
        if discover(node_candidates()).is_none() {
            eprintln!("skipping: no Node interpreter on PATH");
            return;
        }
        let (_d, c) = ctx();
        let out = NodeTool
            .execute(
                serde_json::json!({"command": "console.log(process.argv.slice(1).join(','))", "args": ["yo"]}),
                &c,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "unexpected failure: {}", out.content);
        assert!(out.content.contains("yo"));
    }

    #[tokio::test]
    async fn node_timeout_kills_script() {
        if discover(node_candidates()).is_none() {
            eprintln!("skipping: no Node interpreter on PATH");
            return;
        }
        let (_d, c) = ctx();
        let out = NodeTool
            .execute(
                serde_json::json!({"command": "setTimeout(() => {}, 30000)", "timeout_ms": 500}),
                &c,
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.to_lowercase().contains("timeout"));
    }

    #[tokio::test]
    async fn missing_command_is_error_output() {
        let (_d, c) = ctx();
        assert!(
            PythonTool
                .execute(serde_json::json!({}), &c)
                .await
                .unwrap()
                .is_error
        );
        assert!(
            NodeTool
                .execute(serde_json::json!({}), &c)
                .await
                .unwrap()
                .is_error
        );
    }

    #[tokio::test]
    async fn chatty_output_is_capped_not_buffered_unbounded() {
        if discover(python_candidates()).is_none() {
            eprintln!("skipping: no Python interpreter on PATH");
            return;
        }
        let (_d, c) = ctx();
        // ~1.8MB on stdout, far past STREAM_CAPTURE_CAP: the tool must
        // still return promptly with the usual truncation marker (the cap
        // only bounds buffering, not the visible contract).
        let out = PythonTool
            .execute(
                serde_json::json!({"command": "print('A' * 1800000)", "timeout_ms": 60000}),
                &c,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("[truncated]"));
        // Well under the captured stream: the cap stopped buffering.
        assert!(out.content.len() < 4 * crate::shell_tool::MAX_OUTPUT_BYTES);
    }
}
