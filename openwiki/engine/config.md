---
type: concept
title: 配置系统（wavecode-config）
description: TOML 配置加载、provider 解析与 api key 优先级、凭据 Debug 脱敏。
tags: [config, toml, credentials]
---

# 配置系统（wavecode-config）

## 职责

`wavecode-config` 负责 TOML 配置加载与 provider 解析（`crates/config/src/lib.rs`）。M1 阶段实现：加载用户级 `~/.wavecode/config.toml`，解析 `model` / `model_provider` / `model_providers` 配置段，并解析当前 provider 的 api key。规划中的分层合并（CLI 参数 > 项目级 `.wavecode/config.toml` > 用户级 > 内置默认值）与 `profiles`、`mcp_servers`、`features` 等能力在后续里程碑落地（SPEC §13）。

## 核心类型

- `ProviderKind`（serde kebab-case）：`Anthropic` / `OpenAiCompatible`（M1 仅 Anthropic 有客户端实现）。
- `ProviderConfig { kind, base_url, env_key, api_key, context_window, max_output_tokens }`：
  - `env_key`：指向环境变量名，运行时从该环境变量读取 api key；
  - `api_key`：内联 key（M1 便利项，优先级低于 env_key）；
  - 默认值：`context_window() = 200_000`、`max_output_tokens() = 8192`（llm 侧能力表的 M1 临时承载）。
- `Config { model, model_provider, model_providers: HashMap<String, ProviderConfig> }`。
- `ConfigError`（thiserror）：`NotFound(PathBuf)`（文件不存在或不可读，M1 约定不区分权限问题）/ `Parse(toml::de::Error)` / `MissingProvider(String)` / `MissingApiKey(String)`。

## 加载与解析

- `Config::load()`：`~/.wavecode/config.toml`；home 取 `USERPROFILE`（Windows）兜底 `HOME`，两者皆未设置时按相对路径查找（效果等同 NotFound）。`load_from(path)` 供 `--config` 与测试使用。
- `resolve_provider() -> (&ProviderConfig, String)`：**api key 优先级：`env_key` 指向的环境变量 > 内联 `api_key`**；环境变量存在但为空串（`export KEY=`）视为未设置，回落内联 key——不带空 key 发请求。

## 凭据防护

`ProviderConfig` 的 `Debug` **手写脱敏**：`api_key` 永不显示真实值（`Some` 显示 `***`、`None` 显示 `None`），防日志 / 错误输出泄露密钥；其余字段正常显示。这是 [cli bootstrap](../runtime/cli.md) 与审查报告（review.md §2.2）确认的凭据全链路防护第一环。

## 聚焦测试（`crates/config/src/lib.rs` tests）

| 测试 | 锁定的行为 |
|---|---|
| `parses_minimax_config` | 完整解析：model/provider/base_url/kind/默认窗口与输出上限 |
| `missing_api_key_is_error` | env 未设置且无内联 key → MissingApiKey（ENV_LOCK 互斥，进程级 env 状态） |
| `env_key_takes_precedence` | env_key 环境变量优先于内联 key |
| `inline_key_fallback` | env_key 缺失时回落内联 key |
| `empty_env_key_falls_back_to_inline` | `export KEY=`（空串）视为未设置，回落内联 key |
| `provider_config_debug_redacts_api_key` | `ProviderConfig` 与包含它的 `Config` 的 Debug 输出均不含 key 原文、含 `***`；api_key 为 None 时显示 `None` |
| `malformed_toml_is_parse_error` / `missing_required_field_is_parse_error` | TOML 语法错误 / 缺必填字段 → `Parse` |
| `missing_provider_is_error` / `load_from_missing_file_is_not_found` | 未定义 provider → `MissingProvider`；文件缺失 → `NotFound` |

## 规划（SPEC §13）

- 分层合并：`PartialConfig` 逐层解析（全 Optional），标量项目级覆盖用户级、map 按键合并、数组替换不拼接；CLI 参数最高优先级。
- 新增配置段：`permission_mode`、`profile`、`profiles`、`mcp_servers`、`projects.<dir>` 按目录覆盖、`features`（实验特性开关）。
- M2 待办：TOML Parse 错误行级脱敏（api_key 行语法错误时不回显原文，SPEC §17.5）。

## 相关页面

- 消费方：[命令行入口（wavecode-cli）](../runtime/cli.md)（bootstrap 装配链）、[模型抽象层（wavecode-llm）](llm.md)（provider 解析产物）
- 规划：[规划中的特性 crate（stub）](../planned/feature-crates.md)（sandbox/auth 的配置接入）
