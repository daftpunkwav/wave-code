# Cookbook：添加一个工具

[English](adding-a-tool.md) | 中文

已验证的示例：`web_fetch`（`crates/capabilities/tools/src/web_fetch.rs`，注册于 `Registry::builtin()`）和 `lsp_diagnostics`（`crates/capabilities/tools/src/lsp/tools.rs`，经 `src/lsp.rs` 再导出，在会话装配中迟注册）。

## 1. 实现 `Tool` trait

在 `crates/capabilities/tools/src/` 下添加一个模块并实现 `Tool`（`crates/capabilities/tools/src/lib.rs`）：

```rust
#[async_trait::async_trait]
impl Tool for MyTool {
    fn name(&self) -> &str { "my_tool" }            // 全局唯一；&str，不是 &'static str
    fn description(&self) -> &str { "...English, model-facing..." }
    fn input_schema(&self) -> serde_json::Value { /* JSON Schema */ }
    fn is_read_only(&self) -> bool { true }          // 决定并行性 + plan 模式
    // fn is_destructive(&self) -> bool { false }    // 默认 false；确属破坏性才设置
    // fn validate(&self, input) -> Result<()>       // 语义预检；接口存在，
    //                                               // 但编排尚未调用

    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        // 业务失败：  Ok(ToolOutput { content: reason, is_error: true })
        // 绝不 panic，绝不 Err。Err 只用于实现故障。
    }
}
```

属性的真实性很重要：`is_read_only` 把工具放进并发批次与 explore 子集；`is_destructive = true`（或未注册的名字）则强制走串行、易触发审批的路径。不要为了让写工具更快而把它标成只读。

## 2. 注册它

在两个注册点中选其一：

- **静态内建**：追加到 `crates/capabilities/tools/src/lib.rs` 的 `Registry::builtin()`。工具不需要会话状态时用这个。
- **迟注册**：在 `crates/operations/bootstrap/src/session.rs` 中、它依赖的部件就绪之后注册——LSP 工具展示了这个形态（`register_lsp_tools` 围绕一个共享的 `LspProviders` 句柄注册五个工具），需要围绕驱动器构建子任务服务的 `skill` / `task` 工具同理。注册表是内部可变的，迟注册会到达每一个已共享的句柄（executor、策略、模型适配器），无需重建。

需要会话共享存储的工具遵循 `todo_write` 模式：`with_todo_write` 式的构造函数在工具与会话配置之间共享同一个 `Arc`。

## 3. 策略与允许清单的影响

- 策略永不匹配工具名。裁决来自你的 `is_read_only` / `is_destructive` 属性，经 `PolicyAdapter`（`crates/operations/bootstrap/src/policy_adapter.rs`），再加上 sandbox 的 deny/allow 规则与权限模式。无需额外接线——但可以预期 `shell` 类工具在 `default` 模式下得到 `Ask` 裁决，在 `plan` 模式下除非只读否则直接 `Deny`。
- `Registry::name_subset`（技能 `allowed-tools` fork）与 `read_only_subset`（explore 子 agent）会自动按名收录你的工具；技能 frontmatter 里的拼写错误只会让它不可用。
- 文件系统访问必须走 `path_guard::resolve`（限制在 `ToolCtx::cwd` 之下）；子进程必须经 `sanitize_env`（环境变量清洗）运行并遵守 sandbox 后端的隔离。见 `docs/subsystems/safety.md`。

## 4. 要写的测试

模仿现有工具（`web_fetch.rs` 与 `lsp.rs` 是参考集）：

- **注册与属性**：扩展 `builtin_registers_script_lsp_and_web_fetch` 的模式（`crates/capabilities/tools/src/lib.rs`）——工具存在，且其 `is_read_only` 分类符合只读子集的预期。
- **业务错误是 `Ok(..., is_error: true)`**：缺参数、坏输入、上游"未找到"——断言 `is_error` 且内容解释了失败原因（见 `web_fetch` 在其 `is_error: true` 返回附近的失败路径测试）。
- **故障保持可区分**：如果 `execute` 可能返回 `Err`，断言 `ToolAdapter` 以 `tool fault:` 前缀暴露它（`crates/operations/bootstrap/src/tool_adapter.rs` 的测试给出了形状）。
- **围栏**：任何带路径参数的工具都要有 `path_guard` 逃逸测试（`crates/capabilities/tools/src/path_guard.rs` 中的同缀前缀混淆与符号链接逃逸测试是模板）。
- **迟注册**（如果你选了它）：断言注册后工具经共享的 `Arc<Registry>` 可达（`lib.rs` 中的 `late_registration_reaches_shared_handles`）。

## 5. 端到端检查管线

`cargo test --workspace --locked` 加一次手动回合：工具应出现在 `available_tools` 里（经 `Registry::specs` 按名排序），遵守 plan 模式的只读规则，并产生可被回放折叠配对的 `ToolCallBegin`/`ToolCallEnd` 对——见 `docs/cookbook/recording-and-replaying.md`（中文版 [recording-and-replaying.zh.md](recording-and-replaying.zh.md)）录制一个会话并确认。
