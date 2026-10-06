# 子系统：扩展性

[English](extensibility.md) | 中文

五个扩展面：技能（skills）、钩子（hooks）、MCP、插件（plugins）与命名的 agent 定义。它们全部骑在既有接缝上（注册表、hooks 网关、任务服务），不改循环。

## 技能（`crates/capabilities/skills/src/lib.rs`）

技能是 `<root>/skills/<name>/SKILL.md`——YAML frontmatter 加 Markdown 正文。发现按优先级升序遍历根目录——builtin < `~/.wavecode/skills` < `<cwd>/.wavecode/skills`——同名技能被更高优先级的根覆盖（`standard_roots`）；经 MCP 暴露的技能是规定的第四个来源，目前仍只是占位。必填字段是 `description`；可选字段：`when_to_use`、`allowed-tools`、`context: inline | fork`、`user-invocable`、`argument-hint`、`paths`（serde 别名同时接受 kebab-case 与 snake_case）。

Frontmatter 解析使用 `serde_yaml` 而不是手写扫描器——一个刻意的取舍：技能值可能含冒号、列表与多行字符串，极简解析器的边界情况会无声降级。Frontmatter 字段取 frontmatter 表的交集（`description` 必填，其余可选）。

- **Inline**（默认）：正文展开进当前会话的结果文本，`$ARGUMENTS` 被替换为调用参数。
- **Fork**：技能经任务服务在专用 subagent 中运行；其 `allowed-tools` 限制子 agent 的工具面（没有工具面的 fork 得到注册表面减去生成子任务的工具，因此任何 fork 都不可能再生成子 agent）。

`${WAVECODE_SKILL_DIR}` 展开为技能目录，捆绑的参考文件由此解析。面向模型的技能目录是有预算的——上下文窗口的 1% 作为字符配额，超限时降级截断（先丢 `when_to_use`，再截断描述）——因此大的技能包吃不掉提示词。模型经 `skill` 工具调用技能（`crates/capabilities/skills/src/tool.rs`，与它服务的目录同居一处），因为需要子任务服务而迟注册；`task_output` / `task_stop` 观察并停止 fork 生成的子 agent。

## 钩子（`crates/capabilities/hooks/src/lib.rs`）

八个生命周期点：`PreToolUse`、`PostToolUse`、`UserPromptSubmit`、`SessionStart`、`SessionEnd`、`Stop`、`PreCompact`、`PostCompact`。只有三个可阻塞——`PreToolUse`、`UserPromptSubmit`、`Stop`（退出码 2 否决并把 stderr 喂回模型）；在不可阻塞点上，否决降级为警告，坏钩子无法卡住循环。

两种钩子类型共享一个执行形状（matcher / 命令 / stdin 载荷 / 超时）：

- `command` — 来自配置的 shell 命令（`matcher` 是 `|` 分隔的工具名；放在非工具点上的 matcher 永不触发；超时强杀并警告）。配置条目携带 `matcher` / `command` / `timeout_ms` / `once`（table 或 table array）。
- `prompt` — 经 `HookEngine::register_prompt_hook` 以编程方式注册；stdout 被捕获（有上限，溢出时带 `PROMPT_CONTEXT_TRUNCATED` 标记）并**作为上下文注入**给模型。prompt 钩子从不阻塞。

结构性死条目在加载时被拒绝（空命令、空白 matcher），而不是悄悄地永不触发。

## MCP 客户端（`crates/capabilities/mcp/src/lib.rs`）

传输：子进程 **stdio** 与 **streamable HTTP**（服务器以 404 使会话过期时 HTTP 客户端重新初始化一次；连接断开在下一次调用时重连愈合，连续愈合之间按指数冷却（250ms 起倍增，上限 8s），使宕机的服务器不再每次调用都付出一次进程启动；客户端没有后台重试循环，且 `tools/call` 绝不在愈合时重放，因为服务器可能已经执行过该调用）。列表方法（`tools/list`、`resources/list`、`prompts/list`）在有界范围内逐页遍历，因此行为不端的服务器无法让客户端永远循环。经分页 `tools/list` 发现的工具以 `mcp__{server}__{tool}`（`MCP_TOOL_PREFIX`）桥接进注册表，因此服务器侧的名字不会与内建工具冲突，且桥接工具从不采信服务器自述的 `readOnlyHint`——未知效果保留审批路径。当服务器宣告相应能力时，两个发现工具桥接资源与提示——`mcp__{server}__read_resource`（`resources/list` + `resources/read`）与 `mcp__{server}__get_prompt`（`prompts/list` + `prompts/get`）——服务器的目录嵌入在工具描述中。

已知限制（代码中已写明）：交互式浏览器/PKCE OAuth 不在范围内——认证方式是静态 header 或 OAuth **客户端凭证**（`oauth_token_url` + `oauth_client_id`/`oauth_client_secret`，可选 `oauth_scope`；`oauth_token_url` 在环回接口之外必须为 https——client secret 随请求体传输，因此非环回的明文 http 会在构建期被拒绝；http MCP endpoint 本身必须始终为 https 或环回——streamable-http 交换是有状态的（服务器可能签发随每个后续请求回传的 mcp-session-id 句柄），因此远程明文 http endpoint 无论是否携带凭据都会在构建期被拒绝；`oauth_token_url` 适用同一规则，明文 http 的环回 token endpoint 始终直连（不走代理，否则代理会看到 secret）；铸造出的 bearer 令牌会缓存到临近过期前，铸造失败只重拨一次），客户端拒绝指向交互式流程的质询。MCP prompts 不自动转换为内联技能：skills crate 保留一个无人产出的 `SkillSource::Mcp` 来源变体，且不存在任何转换接线。

## 插件（`crates/runtime/plugin/src/lib.rs`、`crates/capabilities/skills/src/plugin.rs`）

runtime 的 `Registry` 按清单依赖顺序启动插件，逆序停止；`ServiceMap` 以 type-id 键控查找注入类型化服务。发现读取 `runtime.toml` 清单（`{name, version, depends?}` 身份）并按**警告并跳过**降级：无效清单带原因警告并被跳过，绝不让装配失败。skills crate 中的 `PluginLoader` 加载用户插件包（`plugin.toml`：`name`、`version`、可选 `skills_dir`、hooks、MCP 服务器条目），带同样的 warn-and-skip 契约，把它们的技能/钩子/MCP 服务器喂进正常装配。

## 命名的 agent 定义（`crates/capabilities/tools/src/agent_task_tool.rs`）

`task` 工具把自由格式的提示委派给子 agent。`discover_agent_defs(cwd)` 先扫描 `.wavecode/agents/*.md` 再扫描 `.claude/agents/*.md`（跨工具兼容约定）；**同名的第一个定义获胜**，因此仓库级定义遮蔽全局定义。不可解析的文件被跳过——发现是尽力而为的，绝不能让调用失败。

Frontmatter 字段：`name`（回退到文件词干）、`description`、`tools`（逗号分隔或 `- item` 列表）、`kind`（`explore` / `readonly` / `read-only` 选择只读配置）。定义成为生成子 agent 的身份前导词加工具面限制。做法与深度上限一节（`crates/runtime/child/src/lib.rs` 中的 `MAX_CHILD_DEPTH = 3`）见 `docs/cookbook/adding-a-subagent.md`（中文版 [../cookbook/adding-a-subagent.zh.md](../cookbook/adding-a-subagent.zh.md)）。
