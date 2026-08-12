# @wavecode/web

WaveCode Web UI。

- 技术选型：Vite + React + TypeScript（骨架阶段未锁定依赖，见 docs/SPEC.md §8）。
- 通信：WebSocket JSON-RPC 客户端，类型由 `wavecode app-server generate-ts` 生成。
- 与 Desktop 共享同一套 UI 组件与协议客户端，Electron 仅做壳与浏览器自动化桥。
