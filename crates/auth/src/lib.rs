//! wavecode-auth — 认证与凭据管理。
//!
//! 支持 API key 与 OAuth（PKCE + localhost 回调）两种登录方式；
//! 凭据存入系统 keyring（Windows 凭据管理器 / macOS Keychain /
//! Linux Secret Service），按 provider 隔离管理。
