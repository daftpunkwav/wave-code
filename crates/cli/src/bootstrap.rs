//! 启动装配：配置文件 → [`SessionConfig`]。
//!
//! 链路：`Config::load`（或 `--config` 指定）→ `resolve_provider` →
//! `AnthropicClient::new` → [`Boot`]（`Registry::builtin()`、
//! cwd = 当前目录、`--model` 覆盖 config.model）；P6 追加记忆装配：
//! WAVECODE.md 指令记忆收集 + 持久记忆索引快照（经 core 的 memory 模块
//! 再导出，不新增 cli→memory 依赖边）；P9 追加 MCP 装配：`[mcp_servers]`
//! 原始表经 core 转换为已校验清单（仅解析 + 持有，连接留待真实
//! transport）。

use std::path::Path;

use wavecode_config::{Config, ConfigError};
use wavecode_core::SessionConfig;

/// 启动装配错误：配置问题（退出码 2）与其他运行时问题（退出码 1）须可区分。
#[derive(Debug, thiserror::Error)]
pub enum BootError {
    /// 配置缺失 / 解析失败 / provider 或 api key 未定义。
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// 无法确定当前工作目录（tools 的 path_guard 约定 cwd 为绝对路径，不兜底相对路径）。
    #[error("无法确定当前工作目录: {0}")]
    Cwd(std::io::Error),
}

/// 装配产物（[`load_session_config`] 返回值）。
pub struct Boot {
    /// 会话配置（移入 InProcessClient 驱动）。
    pub session: SessionConfig,
    /// 已解析的 MCP server 清单（P9，SPEC §10/§13）：首版仅解析 +
    /// 持有——`/mcp` 命令的展示面；连接与工具注册留待真实 transport
    /// 落地（届时在装配层经 `McpToolBridge` 注册进 registry）。
    pub mcp_servers: Vec<wavecode_core::mcp::NamedMcpServer>,
}

/// 装配 [`Boot`]（会话配置 + MCP server 清单）。
///
/// - `config_path`：`Some` 走 [`Config::load_from`]，`None` 走用户级
///   [`Config::load`]（`~/.wavecode/config.toml`）；
/// - `model_override`：`--model` 值，优先于 `config.model`。
pub fn load_session_config(
    config_path: Option<&Path>,
    model_override: Option<&str>,
) -> Result<Boot, BootError> {
    let config = match config_path {
        Some(path) => Config::load_from(path)?,
        None => Config::load()?,
    };
    let (provider, api_key) = config.resolve_provider()?;
    warn_if_insecure_http(&provider.base_url);

    // M1 仅 AnthropicClient 一种模型实现（provider.kind 的 OpenAiCompatible
    // 分支待后续里程碑的 OpenAI 客户端落地后区分）。
    let model = wavecode_llm::AnthropicClient::new(provider.base_url.clone(), api_key);

    // P2：config.permission_mode → 权限模式；未配置回退 default，
    // 无法识别的值警告后回退（显式、诚实，不做静默放行）。
    let permission_mode = config
        .permission_mode
        .as_deref()
        .map(|raw| {
            wavecode_protocol::PermissionMode::parse(raw).unwrap_or_else(|| {
                eprintln!(
                    "警告：无法识别的 permission_mode = {raw:?}，回退 default\
                     （合法值：default / plan / acceptEdits / bypassPermissions）"
                );
                wavecode_protocol::PermissionMode::Default
            })
        })
        .unwrap_or(wavecode_protocol::PermissionMode::Default);

    let cwd = std::env::current_dir().map_err(BootError::Cwd)?;
    let home = wavecode_core::memory::home_dir();
    // P6：记忆装配（SPEC §5.4/§7）——指令记忆收集（用户级 → 项目根 → cwd）
    // 与持久记忆索引快照；两者注入系统提示词槽位，store_root 供
    // memory_write 与 SessionEnd 自动提取。home 不可解析时退化为无记忆
    // 能力（显式警告，不静默降级）。
    let memory = match &home {
        Some(home) => {
            let instruction =
                wavecode_core::memory::collect_instruction_memory(Some(home.as_path()), &cwd);
            let store_root = wavecode_core::memory::MemoryStore::default_root(home);
            let memory_index = wavecode_core::memory::MemoryStore::new(store_root.clone())
                .read_index()
                .unwrap_or_else(|e| {
                    eprintln!("警告：记忆索引读取失败（按无记忆继续）：{e}");
                    String::new()
                });
            Some(wavecode_core::MemorySessionConfig {
                instruction_memory: instruction.combined,
                memory_index,
                store_root,
            })
        }
        None => {
            eprintln!("警告：无法解析用户主目录（USERPROFILE/HOME），记忆能力不可用");
            None
        }
    };

    // P7：skills 装配（SPEC §8）——按优先级 builtin < 用户级 < 项目级发现
    //（builtin 首版无内置技能目录，留 None；MCP 暴露 skill 随 P9 落地）。
    // 单个坏文件警告跳过（发现产物 warnings），不炸启动；无 skill 时
    // 不挂技能面（skill 工具不注册、清单不注入）。
    let skills = {
        let roots = wavecode_core::skills::standard_roots(None, home.as_deref(), &cwd);
        let discovery = wavecode_core::skills::discover(&roots);
        for warning in &discovery.warnings {
            eprintln!("警告：{warning}");
        }
        if discovery.set.is_empty() {
            None
        } else {
            Some(wavecode_core::SkillSessionConfig {
                set: std::sync::Arc::new(discovery.set),
            })
        }
    };

    // P7：hooks 装配（SPEC §9）——config `[hooks]` 原始表 → HookEngine
    //（core 转换；cli 不新增 cli→hooks/config 细节依赖边）。事件点名非法
    // 显式警告后按无 hooks 继续（不静默——stderr 可见；hooks 是增强面，
    // 配置错误不应阻塞启动）。
    let hooks = match wavecode_core::hooks::engine_from_config(&config.hooks) {
        Ok(engine) if engine.is_empty() => None,
        Ok(engine) => Some(std::sync::Arc::new(engine)),
        Err(e) => {
            eprintln!("警告：hooks 配置无效（按无 hooks 继续）：{e}");
            None
        }
    };

    // P9：MCP server 配置装配（SPEC §10/§13）——config `[mcp_servers]`
    // 原始表经 core 转换为已校验清单（stdio/http 二选一校验）；非法条目
    // 警告跳过，不阻塞启动。首版仅解析 + 持有（/mcp 展示面），连接与
    // 工具注册留待真实 transport 落地。
    let (mcp_servers, mcp_warnings) = wavecode_core::mcp::servers_from_config(&config.mcp_servers);
    for warning in &mcp_warnings {
        eprintln!("警告：{warning}");
    }

    // P10：会话持久化装配（SPEC §16）——rollout 根目录 ~/.wavecode/threads
    // + 新会话分配 uuid thread id（`wavecode resume <id>` 由 main 在 boot
    // 后覆盖为指定 id，构造即 replay 恢复）。home 不可解析 / 目录创建失败
    // 时退化为不持久化（显式警告，与记忆面同纪律；home 警告记忆装配已打印）。
    let rollout = match &home {
        Some(home) => {
            let root = wavecode_core::rollout::default_root(home);
            match std::fs::create_dir_all(&root) {
                Ok(()) => Some(wavecode_core::rollout::RolloutConfig {
                    root,
                    thread_id: uuid::Uuid::new_v4().to_string(),
                }),
                Err(e) => {
                    eprintln!("警告：rollout 目录创建失败（会话不持久化）：{e}");
                    None
                }
            }
        }
        None => None,
    };

    Ok(Boot {
        session: SessionConfig {
            model_name: model_override
                .map(str::to_string)
                .unwrap_or_else(|| config.model.clone()),
            context_window: provider.context_window(),
            max_output_tokens: provider.max_output_tokens(),
            model: std::sync::Arc::new(model),
            registry: wavecode_tools::Registry::builtin(),
            cwd,
            // provider 的 env_key 自定义名（如 MINIMAX_KEY）注入 deny_env：
            // shell 工具的敏感后缀模式挡不住这类名字，须在装配层显式剔除；
            // 未配 / 空串则无需剔除。
            deny_env: provider
                .env_key
                .as_deref()
                .filter(|name| !name.is_empty())
                .map(|name| vec![name.to_owned()])
                .unwrap_or_default(),
            // TODO(P2 后续)：allow/deny 规则表的配置来源（config 分层 §17.5 M3
            // 落地后接线），当前仅权限模式来自 config，规则表为空。
            sandbox: wavecode_sandbox::Sandbox::without_rules(permission_mode),
            // P3：上下文管线（三级阈值 / 压缩）取默认值；阈值与保留条数的
            // 配置化随 config 分层（§17.5 M3）接线。
            context: Default::default(),
            memory,
            skills,
            hooks,
            rollout,
        },
        mcp_servers,
    })
}

/// 判定 base_url 是否为"http 且非 loopback"——该形态下 api key 将明文传输，
/// 需警告；https 与 loopback（127.0.0.1 / localhost / ::1）http 不警告。
fn is_insecure_http_url(base_url: &str) -> bool {
    let Some(rest) = base_url.strip_prefix("http://") else {
        return false; // https 等其他 scheme：不警告
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    // 剥离端口：IPv6 字面量带方括号（[::1]:8080），其余按冒号切。
    let host = match authority
        .strip_prefix('[')
        .and_then(|a| a.split(']').next())
    {
        Some(v6) => v6,
        None => authority.split(':').next().unwrap_or_default(),
    };
    !matches!(host, "localhost" | "127.0.0.1" | "::1")
}

/// http 且非 loopback 的 base_url：stderr 打一行警告（api key 明文传输）。
fn warn_if_insecure_http(base_url: &str) {
    if is_insecure_http_url(base_url) {
        eprintln!(
            "警告：base_url 使用 http 且目标非本机回环（{base_url}），api key 将明文传输；生产环境请改用 https。"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 含内联 api_key 的配置（不依赖真实环境变量）。
    const CFG_INLINE_KEY: &str = r#"
model = "m1"
model_provider = "p1"

[model_providers.p1]
type = "anthropic"
base_url = "https://api.example.com/anthropic"
api_key = "k-inline"
"#;

    /// 含 env_key 自定义名（MINIMAX_KEY——敏感后缀模式挡不住的形态）的配置；
    /// 附内联 key 兜底，测试结果不受真实环境变量影响。
    const CFG_ENV_KEY: &str = r#"
model = "m1"
model_provider = "p1"

[model_providers.p1]
type = "anthropic"
base_url = "https://api.example.com"
env_key = "MINIMAX_KEY"
api_key = "k-inline"
"#;

    fn write_config(dir: &tempfile::TempDir, content: &str) -> std::path::PathBuf {
        let path = dir.path().join("config.toml");
        std::fs::write(&path, content).unwrap();
        path
    }

    /// `--model` 覆盖 config.model；不传则用配置值。
    #[test]
    fn model_override_wins() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, CFG_INLINE_KEY);
        let cfg = load_session_config(Some(&path), Some("m-override"))
            .unwrap()
            .session;
        assert_eq!(cfg.model_name, "m-override");
        let cfg = load_session_config(Some(&path), None).unwrap().session;
        assert_eq!(cfg.model_name, "m1");
    }

    /// 配置文件缺失 → BootError::Config(NotFound) 分支（main 映射退出码 2）。
    #[test]
    fn missing_config_is_config_boot_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.toml");
        assert!(matches!(
            load_session_config(Some(&missing), None),
            Err(BootError::Config(ConfigError::NotFound(_)))
        ));
    }

    /// deny_env 装配（批 C）：config 的 env_key 自定义名注入 SessionConfig。
    #[test]
    fn env_key_injected_into_deny_env() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, CFG_ENV_KEY);
        let cfg = load_session_config(Some(&path), None).unwrap().session;
        assert_eq!(cfg.deny_env, vec!["MINIMAX_KEY".to_owned()]);
    }

    /// permission_mode 装配（P2）：config 值 → SessionConfig.sandbox；
    /// 未配置回退 default；非法值警告并回退 default（不静默放行）。
    #[test]
    fn permission_mode_flows_into_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        // 未配置 → default
        let path = write_config(&dir, CFG_INLINE_KEY);
        let cfg = load_session_config(Some(&path), None).unwrap().session;
        assert_eq!(
            cfg.sandbox.mode(),
            wavecode_protocol::PermissionMode::Default
        );
        // 配置 plan → Plan
        let with_plan = CFG_INLINE_KEY.replacen(
            "model = \"m1\"",
            "model = \"m1\"\npermission_mode = \"plan\"",
            1,
        );
        let path = write_config(&dir, &with_plan);
        let cfg = load_session_config(Some(&path), None).unwrap().session;
        assert_eq!(cfg.sandbox.mode(), wavecode_protocol::PermissionMode::Plan);
        // 非法值 → 回退 default
        let with_bad = CFG_INLINE_KEY.replacen(
            "model = \"m1\"",
            "model = \"m1\"\npermission_mode = \"yolo\"",
            1,
        );
        let path = write_config(&dir, &with_bad);
        let cfg = load_session_config(Some(&path), None).unwrap().session;
        assert_eq!(
            cfg.sandbox.mode(),
            wavecode_protocol::PermissionMode::Default
        );
    }

    /// 无 env_key / env_key 为空串 → deny_env 为空。
    #[test]
    fn no_env_key_gives_empty_deny_env() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, CFG_INLINE_KEY);
        let cfg = load_session_config(Some(&path), None).unwrap().session;
        assert!(cfg.deny_env.is_empty());
        let path = write_config(
            &dir,
            &CFG_ENV_KEY.replace("env_key = \"MINIMAX_KEY\"", "env_key = \"\""),
        );
        let cfg = load_session_config(Some(&path), None).unwrap().session;
        assert!(cfg.deny_env.is_empty());
    }

    /// MCP 装配（P9）：`[mcp_servers]` 解析 + 持有进 Boot；非法条目
    /// 警告跳过（不阻塞启动），合法条目保留 stdio/http 形态。
    #[test]
    fn mcp_servers_parsed_and_held() {
        let dir = tempfile::tempdir().unwrap();
        let toml = format!(
            r#"{CFG_INLINE_KEY}
[mcp_servers.playwright]
command = "npx"
args = ["@playwright/mcp@latest"]

[mcp_servers.remote]
url = "https://mcp.example.com/sse"

[mcp_servers.broken]
command = "x"
url = "https://y"
"#
        );
        let path = write_config(&dir, &toml);
        let boot = load_session_config(Some(&path), None).unwrap();
        assert_eq!(boot.mcp_servers.len(), 2, "非法条目跳过");
        assert_eq!(boot.mcp_servers[0].name, "playwright");
        assert_eq!(boot.mcp_servers[0].config.transport_kind(), "stdio");
        assert_eq!(boot.mcp_servers[1].name, "remote");
        assert_eq!(boot.mcp_servers[1].config.transport_kind(), "http");
        // 未配置 → 空清单。
        let path = write_config(&dir, CFG_INLINE_KEY);
        let boot = load_session_config(Some(&path), None).unwrap();
        assert!(boot.mcp_servers.is_empty());
    }

    /// http 警告的判定面：仅"http 且非 loopback"为真。
    #[test]
    fn insecure_http_detection() {
        assert!(is_insecure_http_url("http://api.example.com"));
        assert!(is_insecure_http_url("http://192.168.1.10:8080/v1"));
        assert!(!is_insecure_http_url("https://api.example.com"));
        // 前缀形似 loopback 的域名不是 loopback。
        assert!(is_insecure_http_url("http://127.0.0.1.evil.example.com"));
        // loopback http 不警告（本地调试形态）。
        assert!(!is_insecure_http_url("http://127.0.0.1:8080"));
        assert!(!is_insecure_http_url("http://localhost:3000/v1"));
        assert!(!is_insecure_http_url("http://[::1]:9000"));
    }
}
