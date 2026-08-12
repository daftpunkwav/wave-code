# @wavecode/desktop

WaveCode Desktop。

- 技术选型：Electron；渲染层复用 `@wavecode/web` 的 UI 与协议客户端。
- 核心差异能力：内置浏览器视图（`webContents`），通过 `webContents.debugger`
  （Chrome DevTools Protocol）向 core 注册浏览器自动化工具
  （navigate / click / type / screenshot / snapshot 等）。
- 与 core 通信：以 stdio 子进程方式启动并连接 `wavecode app-server`。
