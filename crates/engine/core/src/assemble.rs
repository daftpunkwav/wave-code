//! 启动装配（2026-08 自 cli bootstrap 收口）：config 文件 → [`SessionConfig`]。
//!
//! 装配是引擎职责：provider 解析、记忆/技能/hooks/MCP/rollout 的组装都
//! 在本模块单点完成——cli 退化为参数解析与呈现，第二个前端（Desktop/Web
//! 直连 app-server）出现时零复制复用。cli→capabilities 的直连依赖边随之
//! 移除（SPEC §3 矩阵 cli 行收敛为 app-server + config + core + protocol + tui）。
//!
//! 警告不直接打 stderr：收集进 [`Boot::warnings`]，呈现方式由调用方决定
//! （cli 打 stderr；事件型前端转 Warning 事件）。

use std::path::Path;

use wavecode_config::{Config, ConfigError};

use crate::SessionConfig;

/// 启动装配错误：配置问题（cli 退出码 2）与其他运行时问题（退出码 1）须可区分。
#[derive(Debug, thiserror::Error)]
pub enum BootError {
    /// 配置缺失 / 解析失败 / provider 或 api key 未定义。
    #[error(transparent)]
    Config(#[from] ConfigError),
}

/// 装配产物（[`load_boot`] 返回值）。
pub struct Boot {
    /// 会话配置（移入驱动方：cli 经 InProcessClient spawn）。
    pub session: SessionConfig,
    /// 已解析的 MCP server 清单（P9，SPEC §10/§13）：首版仅解析 +
    /// 持有——`/mcp` 命令的展示面；连接与工具注册留待真实 transport
    /// 落地（届时在装配层经 `McpToolBridge` 注册进 registry）。
    pub mcp_servers: Vec<crate::mcp::NamedMcpServer>,
    /// 装配期警告（http 明文风险 / 非法 permission_mode / home 缺失 /
    /// skills 发现 / hooks 配置 / MCP 条目 / rollout 目录创建）：文案不含
    /// "警告："前缀，由调用方统一包装。
    pub warnings: Vec<String>,
}

/// 装配 [`Boot`]（会话配置 + MCP server 清单 + 警告）。
///
/// - `config_path`：`Some` 走 [`Config::load_from`]，`None` 走用户级
///   [`Config::load`]（`~/.wavecode/config.toml`）；
/// - `model_override`：`--model` 值，优先于 `config.model`；
/// - `cwd` / `home`：工作目录（path_guard 约定绝对路径）与用户主目录
///   （`None` = 记忆 / rollout 能力不可用），由调用方从运行环境解析——
///   core 不读进程环境。
pub fn load_boot(
    config_path: Option<&Path>,
    model_override: Option<&str>,
    cwd: &Path,
    home: Option<&Path>,
) -> Result<Boot, BootError> {
    let config = match config_path {
        Some(path) => Config::load_from(path)?,
        None => Config::load()?,
    };
    let (provider, api_key) = config.resolve_provider()?;
    let mut warnings = Vec::new();
    if is_insecure_http_url(&provider.base_url) {
        warnings.push(format!(
            "base_url 使用 http 且目标非本机回环（{}），api key 将明文传输；生产环境请改用 https。",
            provider.base_url
        ));
    }

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
                warnings.push(format!(
                    "无法识别的 permission_mode = {raw:?}，回退 default\
                     （合法值：default / plan / acceptEdits / bypassPermissions）"
                ));
                wavecode_protocol::PermissionMode::Default
            })
        })
        .unwrap_or(wavecode_protocol::PermissionMode::Default);

    // P6：记忆装配（SPEC §5.4/§7）——指令记忆收集（用户级 → 项目根 → cwd）
    // 与持久记忆索引快照；两者注入系统提示词槽位，store_root 供
    // memory_write 与 SessionEnd 自动提取。home 不可解析时退化为无记忆
    // 能力（显式警告，不静默降级）。
    let memory = match home {
        Some(home) => {
            let instruction = wavecode_memory::collect(Some(home), cwd);
            let store_root = wavecode_memory::MemoryStore::default_root(home);
            let memory_index = wavecode_memory::MemoryStore::new(store_root.clone())
                .read_index()
                .unwrap_or_else(|e| {
                    warnings.push(format!("记忆索引读取失败（按无记忆继续）：{e}"));
                    String::new()
                });
            Some(crate::MemorySessionConfig {
                instruction_memory: instruction.combined,
                memory_index,
                store_root,
            })
        }
        None => {
            warnings.push("无法解析用户主目录（USERPROFILE/HOME），记忆能力不可用".to_owned());
            None
        }
    };

    // P7：skills 装配（SPEC §8）——按优先级 builtin < 用户级 < 项目级发现
    //（builtin 首版无内置技能目录，留 None；MCP 暴露 skill 随真实
    // transport 接线）。单个坏文件警告跳过（发现产物 warnings），不炸
    // 启动；无 skill 时不挂技能面（skill 工具不注册、清单不注入）。
    let skills = {
        let roots = wavecode_skills::standard_roots(None, home, cwd);
        let discovery = wavecode_skills::discover(&roots);
        warnings.extend(discovery.warnings.iter().cloned());
        if discovery.set.is_empty() {
            None
        } else {
            Some(crate::SkillSessionConfig {
                set: std::sync::Arc::new(discovery.set),
            })
        }
    };

    // P7：hooks 装配（SPEC §9）——config `[hooks]` 原始表 → HookEngine
    //（core 内转换）。事件点名非法显式警告后按无 hooks 继续（不静默——
    // 警告可见；hooks 是增强面，配置错误不应阻塞启动）。
    let hooks = match crate::hooks::engine_from_config(&config.hooks) {
        Ok(engine) if engine.is_empty() => None,
        Ok(engine) => Some(std::sync::Arc::new(engine)),
        Err(e) => {
            warnings.push(format!("hooks 配置无效（按无 hooks 继续）：{e}"));
            None
        }
    };

    // P9：MCP server 配置装配（SPEC §10/§13）——config `[mcp_servers]`
    // 原始表转换为已校验清单（stdio/http 二选一校验）；非法条目警告跳过，
    // 不阻塞启动。首版仅解析 + 持有（/mcp 展示面），连接与工具注册留待
    // 真实 transport 落地。
    let (mcp_servers, mcp_warnings) = crate::mcp::servers_from_config(&config.mcp_servers);
    warnings.extend(mcp_warnings);

    // P10：会话持久化装配（SPEC §16）——rollout 根目录 ~/.wavecode/threads
    // + 新会话分配 uuid thread id（`wavecode resume <id>` 由 cli 在 boot
    // 后覆盖为指定 id，构造即 replay 恢复）。home 不可解析 / 目录创建失败
    // 时退化为不持久化（显式警告，与记忆面同纪律；home 警告记忆装配已收集）。
    let rollout = match home {
        Some(home) => {
            let root = crate::rollout::default_root(home);
            match std::fs::create_dir_all(&root) {
                Ok(()) => Some(crate::rollout::RolloutConfig {
                    root,
                    thread_id: uuid::Uuid::new_v4().to_string(),
                }),
                Err(e) => {
                    warnings.push(format!("rollout 目录创建失败（会话不持久化）：{e}"));
                    None
                }
            }
        }
        None => None,
    };

    Ok(Boot {
        session: SessionConfig::builder(
            model_override
                .map(str::to_string)
                .unwrap_or_else(|| config.model.clone()),
            std::sync::Arc::new(model),
            wavecode_tools::Registry::builtin(),
            cwd.to_path_buf(),
        )
        .context_window(provider.context_window())
        .max_output_tokens(provider.max_output_tokens())
        // provider 的 env_key 自定义名（如 MINIMAX_KEY）注入 deny_env：
        // shell 工具的敏感后缀模式挡不住这类名字，须在装配层显式剔除；
        // 未配 / 空串则无需剔除。
        .deny_env(
            provider
                .env_key
                .as_deref()
                .filter(|name| !name.is_empty())
                .map(|name| vec![name.to_owned()])
                .unwrap_or_default(),
        )
        // TODO(P2 后续)：allow/deny 规则表的配置来源（config 分层 §17.5 M3
        // 落地后接线），当前仅权限模式来自 config，规则表为空。
        .sandbox(wavecode_sandbox::Sandbox::without_rules(permission_mode))
        // P3：上下文管线（三级阈值 / 压缩）取 builder 默认值；阈值与保留条数
        // 的配置化随 config 分层（§17.5 M3）接线。
        .memory(memory)
        .skills(skills)
        .hooks(hooks)
        .rollout(rollout)
        .build(),
        mcp_servers,
        warnings,
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

    fn boot_with(dir: &tempfile::TempDir, content: &str) -> Boot {
        let path = write_config(dir, content);
        load_boot(Some(&path), None, dir.path(), None).unwrap()
    }

    /// `--model` 覆盖 config.model；不传则用配置值。
    #[test]
    fn model_override_wins() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, CFG_INLINE_KEY);
        let cfg = load_boot(Some(&path), Some("m-override"), dir.path(), None)
            .unwrap()
            .session;
        assert_eq!(cfg.model_name, "m-override");
        let cfg = load_boot(Some(&path), None, dir.path(), None)
            .unwrap()
            .session;
        assert_eq!(cfg.model_name, "m1");
    }

    /// 配置文件缺失 → BootError::Config(NotFound) 分支（cli 映射退出码 2）。
    #[test]
    fn missing_config_is_config_boot_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.toml");
        assert!(matches!(
            load_boot(Some(&missing), None, dir.path(), None),
            Err(BootError::Config(ConfigError::NotFound(_)))
        ));
    }

    /// deny_env 装配：config 的 env_key 自定义名注入 SessionConfig。
    #[test]
    fn env_key_injected_into_deny_env() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = boot_with(&dir, CFG_ENV_KEY).session;
        assert_eq!(cfg.deny_env, vec!["MINIMAX_KEY".to_owned()]);
    }

    /// permission_mode 装配（P2）：config 值 → SessionConfig.sandbox；
    /// 未配置回退 default；非法值警告并回退 default（不静默放行）。
    #[test]
    fn permission_mode_flows_into_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        // 未配置 → default
        let boot = boot_with(&dir, CFG_INLINE_KEY);
        assert_eq!(
            boot.session.sandbox.mode(),
            wavecode_protocol::PermissionMode::Default
        );
        // 配置 plan → Plan
        let with_plan = CFG_INLINE_KEY.replacen(
            "model = \"m1\"",
            "model = \"m1\"\npermission_mode = \"plan\"",
            1,
        );
        let boot = boot_with(&dir, &with_plan);
        assert_eq!(
            boot.session.sandbox.mode(),
            wavecode_protocol::PermissionMode::Plan
        );
        // 非法值 → 回退 default，警告收集（不静默）
        let with_bad = CFG_INLINE_KEY.replacen(
            "model = \"m1\"",
            "model = \"m1\"\npermission_mode = \"yolo\"",
            1,
        );
        let boot = boot_with(&dir, &with_bad);
        assert_eq!(
            boot.session.sandbox.mode(),
            wavecode_protocol::PermissionMode::Default
        );
        assert!(
            boot.warnings.iter().any(|w| w.contains("permission_mode")),
            "非法模式应有警告: {:?}",
            boot.warnings
        );
    }

    /// 无 env_key / env_key 为空串 → deny_env 为空。
    #[test]
    fn no_env_key_gives_empty_deny_env() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = boot_with(&dir, CFG_INLINE_KEY).session;
        assert!(cfg.deny_env.is_empty());
        let path = write_config(
            &dir,
            &CFG_ENV_KEY.replace("env_key = \"MINIMAX_KEY\"", "env_key = \"\""),
        );
        let cfg = load_boot(Some(&path), None, dir.path(), None)
            .unwrap()
            .session;
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
        let boot = load_boot(Some(&path), None, dir.path(), None).unwrap();
        assert_eq!(boot.mcp_servers.len(), 2, "非法条目跳过");
        assert_eq!(boot.mcp_servers[0].name, "playwright");
        assert_eq!(boot.mcp_servers[0].config.transport_kind(), "stdio");
        assert_eq!(boot.mcp_servers[1].name, "remote");
        assert_eq!(boot.mcp_servers[1].config.transport_kind(), "http");
        assert!(
            boot.warnings
                .iter()
                .any(|w| w.contains("broken") || w.contains("MCP")),
            "非法条目应有警告: {:?}",
            boot.warnings
        );
        // 未配置 → 空清单。
        let path = write_config(&dir, CFG_INLINE_KEY);
        let boot = load_boot(Some(&path), None, dir.path(), None).unwrap();
        assert!(boot.mcp_servers.is_empty());
    }

    /// home 缺失 → 记忆 / rollout 能力不可用（警告收集，不静默降级）。
    #[test]
    fn missing_home_degrades_memory_and_rollout() {
        let dir = tempfile::tempdir().unwrap();
        let boot = boot_with(&dir, CFG_INLINE_KEY);
        assert!(boot.session.memory.is_none());
        assert!(boot.session.rollout.is_none());
        assert!(
            boot.warnings.iter().any(|w| w.contains("记忆能力不可用")),
            "{:?}",
            boot.warnings
        );
    }

    /// home 提供 → 记忆面 / rollout 装配就绪（rollout 目录创建在 tempdir 下）。
    #[test]
    fn home_provided_wires_memory_and_rollout() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let path = write_config(&dir, CFG_INLINE_KEY);
        let boot = load_boot(Some(&path), None, dir.path(), Some(home.path())).unwrap();
        assert!(boot.session.memory.is_some());
        let rollout = boot.session.rollout.expect("rollout 应装配");
        assert!(rollout.root.starts_with(home.path()));
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
