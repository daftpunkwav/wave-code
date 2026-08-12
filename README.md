# WaveCode

多平台 AI coding agent：CLI / TUI / Web / Desktop 四端共用单一 Rust agent 核心，
经统一 JSON-RPC 协议（app-server）接入。

## 文档

- [docs/PRD.md](docs/PRD.md) — 产品需求文档（愿景、功能分级、里程碑、验收标准）
- [docs/SPEC.md](docs/SPEC.md) — 技术规格（架构、协议、agent loop、配置与安全模型）

## 仓库结构

- `crates/` — Rust workspace（15 个 crate，core 唯一实现 agent 逻辑）
- `apps/web/` — Web UI（React SPA，WebSocket 接入）
- `apps/desktop/` — Desktop（Electron，内置浏览器 + CDP 自动化）
- `sdk/typescript/` — TypeScript SDK

## 构建

```bash
cargo build -p wavecode-cli
# TS 侧需 pnpm（corepack enable 后 pnpm install）
```

## 使用

先创建 `~/.wavecode/config.toml`（无配置启动时会打印创建指引与内容模板）：

```bash
wavecode exec "<prompt>"   # 非交互单 turn；--json 时 stdout 输出 JSONL 事件流
wavecode                   # 交互 REPL（/quit 退出）
```

## License

MIT，见 [LICENSE](LICENSE)。
