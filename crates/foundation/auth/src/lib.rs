//! wavecode-auth — 认证与凭据管理（**未实现 stub**）。
//!
//! 目标形态（SPEC §14，规划中，当前 crate 无任何实现与消费者）：
//! 支持 API key 与 OAuth（PKCE + localhost 回调）两种登录方式；凭据存入
//! 系统 keyring（Windows 凭据管理器 / macOS Keychain / Linux Secret
//! Service），按 provider 隔离管理。
//!
//! 当前生产凭据走 config 的 `env_key` / `api_key` 字段（wavecode-config）；
//! 本 crate 在 auth 落地前仅保留架构位，删除不影响任何路径。
