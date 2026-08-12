//! shell 工具：跨平台命令执行（非交互：stdin 置空；stdout/stderr 管道捕获）。
//! 失败语义同 fs_tools：业务失败（非零退出码、超时、spawn 失败、参数缺失/类型错）
//! 返回 `Ok(is_error=true)` 把原因回给模型；`Err` 仅用于实现级故障。
//!
//! 已知限制（M2 跟踪）：`kill_on_drop` 只杀 shell 进程自身；孙进程在 spawn 时已
//! 继承 cmd 的管道句柄副本（命令行重定向无法阻止），shell 被杀后成为孤儿继续存活
//! ——真正的变量是孤儿孙进程的存活时长。生产风险：wavecode 退出 drop tokio 运行时
//! 时可能阻塞任意久（如孙进程是 dev server）。正解是进程组级回收（Windows Job
//! Object / Unix killpg），M2 处理。

use std::time::Duration;

use serde_json::{Value, json};

use crate::{Result, Tool, ToolCtx, ToolOutput};

/// 默认超时：60 s。
const DEFAULT_TIMEOUT_MS: u64 = 60_000;
/// 超时上限：300 s，超过按上限钳制。
const MAX_TIMEOUT_MS: u64 = 300_000;
/// stdout / stderr 各自输出上限：30 KB。
const MAX_OUTPUT_BYTES: usize = 30 * 1024;

/// 构造业务失败输出：原因回灌给模型自我纠正。
fn err_output(reason: impl Into<String>) -> ToolOutput {
    ToolOutput {
        content: reason.into(),
        is_error: true,
    }
}

/// 选定 shell 程序与"执行命令串"参数。
///
/// 默认按平台：Windows `cmd /C`、Unix `sh -c`。`WAVECODE_SHELL` 存在时覆盖为用户
/// 自定义 shell（值作为程序路径）。参数风格用启发式：值含 `cmd` 按 `/C` 处理，否则按
/// 类 Unix 的 `-c` 处理——覆盖 cmd / powershell / bash / zsh 等常见命名即可，对非常规
/// 命名可能猜错；保持简单，后续按需改为显式配置。
fn shell_invocation() -> (String, &'static str) {
    if let Ok(custom) = std::env::var("WAVECODE_SHELL") {
        if custom.to_lowercase().contains("cmd") {
            return (custom, "/C");
        }
        return (custom, "-c");
    }
    if cfg!(windows) {
        ("cmd".to_owned(), "/C")
    } else {
        ("sh".to_owned(), "-c")
    }
}

/// 解码并截断单路输出：UTF-8 边界安全，超限追加 `[truncated]`。
fn truncate_output(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    if text.len() <= MAX_OUTPUT_BYTES {
        return text.into_owned();
    }
    let mut cut = MAX_OUTPUT_BYTES;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n[truncated]", &text[..cut])
}

/// spawn 前剔除敏感环境变量，防模型经 `set` / `env` / `echo %VAR%` 读出密钥。
///
/// 两层防护：
/// 1. `ctx.deny_env` 显式名单（装配层注入 provider 的 `env_key`，如 `MINIMAX_API_KEY`）；
/// 2. 敏感后缀模式兜底：变量名以 `_API_KEY` / `_TOKEN` / `_SECRET` / `_PASSWORD`
///    结尾（大小写不敏感）的一律剔除，未配 deny_env 时也能挡住常见密钥形态。
///
/// 威胁模型边界：只防"经子进程环境继承泄密"；`type config.toml` 直读内联
/// api_key 属 M1 已接受面（M1 审查记录在案），不在此防护范围。
fn sanitize_env(cmd: &mut tokio::process::Command, ctx: &ToolCtx) {
    for name in &ctx.deny_env {
        cmd.env_remove(name);
    }
    const SENSITIVE_SUFFIXES: [&str; 4] = ["_API_KEY", "_TOKEN", "_SECRET", "_PASSWORD"];
    for (key, _) in std::env::vars_os() {
        let upper = key.to_string_lossy().to_uppercase();
        if SENSITIVE_SUFFIXES.iter().any(|s| upper.ends_with(s)) {
            cmd.env_remove(&key);
        }
    }
}

/// 执行 shell 命令（写入类工具：可能改文件、起进程，需串行调度）。
pub struct Shell;

#[async_trait::async_trait]
impl Tool for Shell {
    fn name(&self) -> &str {
        "shell"
    }

    fn description(&self) -> &str {
        "Run a shell command in the working directory. The default shell is platform-dependent \
         (cmd /C on Windows, sh -c on Unix; override with the WAVECODE_SHELL env var). \
         Use timeout_ms to bound execution (default 60000 ms, clamped to 300000 ms); on timeout \
         the process is killed. stdout and stderr are captured separately, each truncated at 30KB. \
         The command runs non-interactive (stdin is closed)."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Shell command to execute in the working directory"
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
        let command = match input.get("command").and_then(Value::as_str) {
            Some(c) => c,
            None => {
                return Ok(err_output(
                    "missing or invalid parameter 'command' (string required)",
                ));
            }
        };
        let timeout_ms = match input.get("timeout_ms") {
            None | Some(Value::Null) => DEFAULT_TIMEOUT_MS,
            Some(v) => match v.as_u64() {
                Some(n) => n.min(MAX_TIMEOUT_MS),
                None => {
                    return Ok(err_output(
                        "invalid parameter 'timeout_ms' (non-negative integer required)",
                    ));
                }
            },
        };

        let (program, flag) = shell_invocation();
        let mut cmd = tokio::process::Command::new(program);
        cmd.arg(flag)
            .arg(command)
            .current_dir(&ctx.cwd)
            // 非交互：stdin 置空，防交互式命令（read/pause/npm init）抢宿主终端输入
            .stdin(std::process::Stdio::null())
            // wait_with_output 仅收集 piped 的流；默认 inherit 会读到空
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // 关键：配合 timeout 取消语义——future 被 drop 时自动 kill 子进程
            // （仅 shell 自身；孙进程孤儿化的限制见模块文档）
            .kill_on_drop(true);
        // 脱敏必须在 spawn 前：剔除 deny_env 名单与敏感后缀变量，防泄密
        sanitize_env(&mut cmd, ctx);
        // timeout 包裹整个执行（spawn + 输出读取）；wait_with_output 并发收两路输出，
        // 不会因管道缓冲区满而死锁。
        let run = async { cmd.spawn()?.wait_with_output().await };
        let output = match tokio::time::timeout(Duration::from_millis(timeout_ms), run).await {
            Ok(Ok(output)) => output,
            // 超时：run future 已被 drop，kill_on_drop 保证进程被杀
            Err(_) => {
                return Ok(err_output(format!(
                    "timeout after {timeout_ms}ms: {command}"
                )));
            }
            // spawn 失败（如 shell 不存在）属业务输出，回给模型而非 Err
            Ok(Err(e)) => {
                return Ok(err_output(format!("failed to spawn shell: {e}")));
            }
        };

        // Unix 下被信号杀死时 code() 为 None，记 -1（仍属非零，is_error 成立）。
        let code = output.status.code().unwrap_or(-1);
        let stdout = truncate_output(&output.stdout);
        let stderr = truncate_output(&output.stderr);
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Tool, ToolCtx};

    fn ctx() -> (tempfile::TempDir, ToolCtx) {
        let dir = tempfile::tempdir().unwrap();
        let c = ToolCtx {
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
        };
        (dir, c)
    }

    #[tokio::test]
    async fn captures_stdout_and_exit_code() {
        let (_d, c) = ctx();
        let out = Shell
            .execute(serde_json::json!({"command": "echo hello"}), &c)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("hello"));
        assert!(out.content.contains("exit code: 0"));
    }

    #[tokio::test]
    async fn nonzero_exit_is_error_but_captured() {
        let (_d, c) = ctx();
        let out = Shell
            .execute(serde_json::json!({"command": "exit 3"}), &c)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("exit code: 3"));
    }

    #[tokio::test]
    async fn respects_timeout() {
        let (_d, c) = ctx();
        let cmd = if cfg!(windows) {
            // cmd 内建忙等：不 spawn 孙进程，超时 kill 后无孤儿残留（ping 会留孙进程）
            "for /l %i in (1,1,1000000000) do @rem"
        } else {
            "sleep 10"
        };
        let out = Shell
            .execute(serde_json::json!({"command": cmd, "timeout_ms": 500}), &c)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.to_lowercase().contains("timeout"));
    }

    #[tokio::test]
    async fn captures_stderr() {
        let (_d, c) = ctx();
        let cmd = if cfg!(windows) {
            "echo err 1>&2"
        } else {
            "echo err >&2"
        };
        let out = Shell
            .execute(serde_json::json!({"command": cmd}), &c)
            .await
            .unwrap();
        assert!(out.content.contains("stderr"));
        assert!(out.content.contains("err"));
    }

    #[tokio::test]
    async fn missing_command_is_error_output() {
        let (_d, c) = ctx();
        let out = Shell.execute(serde_json::json!({}), &c).await.unwrap();
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn runs_in_cwd() {
        let (_d, c) = ctx();
        std::fs::write(c.cwd.join("marker.txt"), "x").unwrap();
        let cmd = if cfg!(windows) {
            "dir /b marker.txt"
        } else {
            "ls marker.txt"
        };
        let out = Shell
            .execute(serde_json::json!({"command": cmd}), &c)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("marker.txt"));
    }

    #[test]
    fn truncate_cuts_at_char_boundary_without_mojibake() {
        // 多字节字符（'€' 3 字节）恰跨 30 KB 边界：1 字节前缀使边界落在字符内部
        let text = format!("a{}", "€".repeat(10240)); // 1 + 30720 字节
        let out = truncate_output(text.as_bytes());
        assert!(out.ends_with("[truncated]"));
        // 截断回退到字符边界：不切碎 '€'，无替换字符乱码（U+FFFD）
        assert!(!out.contains('\u{FFFD}'));
        let body = out.strip_suffix("\n[truncated]").unwrap();
        assert!(body.len() <= MAX_OUTPUT_BYTES);
        // String 类型本身保证合法 UTF-8；边界确实回退（30720 非边界 → 30718）
        assert_eq!(body.len(), 1 + 3 * 10239);
    }

    #[test]
    fn truncate_handles_invalid_utf8_without_panic() {
        // 全 0xFF 无效字节：from_utf8_lossy 逐字节替换为 U+FFFD，不得 panic
        let bytes = vec![0xFF; MAX_OUTPUT_BYTES + 100];
        let out = truncate_output(&bytes);
        assert!(out.ends_with("[truncated]"));
        assert!(out.len() <= MAX_OUTPUT_BYTES + "\n[truncated]".len());
    }

    // 环境变量是进程级状态，依赖它的测试需互斥执行（同 config crate 模式）。
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[tokio::test]
    // 故意持锁跨 await：互斥的是进程级 env 状态，须独占整个测试期间
    #[allow(clippy::await_holding_lock)]
    async fn shell_strips_sensitive_env_but_keeps_normal() {
        let _guard = ENV_LOCK.lock().unwrap();
        // 三类变量：后缀模式命中、deny_env 名单命中、普通变量不受影响
        unsafe {
            std::env::set_var("FOO_API_KEY", "secret123");
            std::env::set_var("FOO_PROVIDER_KEY", "secret456");
            std::env::set_var("FOO_NORMAL", "visible789");
        }
        let (_d, mut c) = ctx();
        c.deny_env = vec!["FOO_PROVIDER_KEY".to_owned()];
        // 一条命令打印三者：被剔除的变量展开为空（sh）或原样字面量（cmd）
        let cmd = if cfg!(windows) {
            "echo %FOO_API_KEY%& echo %FOO_PROVIDER_KEY%& echo %FOO_NORMAL%"
        } else {
            "echo \"$FOO_API_KEY\"; echo \"$FOO_PROVIDER_KEY\"; echo \"$FOO_NORMAL\""
        };
        let out = Shell
            .execute(serde_json::json!({"command": cmd}), &c)
            .await
            .unwrap();
        unsafe {
            std::env::remove_var("FOO_API_KEY");
            std::env::remove_var("FOO_PROVIDER_KEY");
            std::env::remove_var("FOO_NORMAL");
        }
        // 密钥不泄露：后缀模式与 deny_env 名单均被剔除
        assert!(!out.content.contains("secret123"));
        assert!(!out.content.contains("secret456"));
        // 白名单不误伤：普通变量子进程仍可见
        assert!(out.content.contains("visible789"));
    }
}
