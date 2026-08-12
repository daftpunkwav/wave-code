# WaveCode

多平台 AI coding agent：CLI / TUI / Web / Desktop 四端共用单一 Rust agent 核心，经统一 JSON-RPC 协议（app-server）接入。

> 🚧 开发中，尚未发布稳定版本。

## 构建与使用

```bash
cargo build -p wavecode-cli

wavecode exec "<prompt>"   # 单 turn 执行
wavecode                   # 交互模式
```

首次使用需创建 `~/.wavecode/config.toml`（无配置启动时会打印创建指引与模板）。

## License

MIT，见 [LICENSE](LICENSE)。
