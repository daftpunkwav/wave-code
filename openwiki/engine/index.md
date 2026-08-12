# 文件

- [配置系统（wavecode-config）](config.md) - TOML 配置加载、provider 解析与 api key 优先级、凭据 Debug 脱敏。
- [Agent 引擎（wavecode-core）](core.md) - Session 生命周期与 turn 状态机循环、中断语义、工具编排与 ToolResult 配对约束。
- [模型抽象层（wavecode-llm）](llm.md) - ChatModel trait、公共消息类型、Anthropic SSE 流式客户端与解析管道。
- [工具系统（wavecode-tools）](tools.md) - Tool trait 与注册表、五个内置工具、路径防逃逸与 shell 执行的安全边界。
