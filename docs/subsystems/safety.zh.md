# 子系统：安全

[English](safety.md) | 中文

安全是分层的：词汇（`crates/foundation/protocol`）、审批门（`crates/safety/gate`）、带规则求值的权限 sandbox（`crates/capabilities/sandbox`），以及同一 crate 内的 OS 隔离后端。runner 经 `PolicyDecider` / `ApprovalSource` 接缝消费这一切。

## 权限模式与策略

`crates/foundation/protocol/src/lib.rs` 拥有带锁定 wire 字符串的 `PermissionMode`：`plan`（仅只读工具；其余直接拒绝并回给模型；系统提示词引导模型经 plan 工具提 proposal）、`auto`（命令执行与破坏性工具逐次询问；文件编辑与其他非执行写直接放行）、`wave`（批准一切——deny 规则仍然生效）。单元测试锁定 serde 标签；`parse` 拒绝大小写漂移，并把旧名（`guarded`/`default`/`acceptEdits` → `auto`，`bypassPermissions`/`yolo` → `wave`）映射到其后继。注意改名风险：`auto` 曾经意味着"批准一切"（即现在 `wave` 的含义），因此旧配置文件只会落得保守一档，绝不会更宽松。

权限规则在 `crates/capabilities/sandbox/src/lib.rs` 中是纯数据：`Rule` 解析精确与 `prefix*` 通配条目（`Rule::parse`），求值是 deny 优先——deny 规则在所有模式下先于 allow 规则或模式默认值生效，因此未被匹配的输入绝不可能获得超过其模式允许的权限（完整判定顺序见下文"sandbox 判定"）。`is_covered_by` 保守地检测被 deny 规则遮蔽的 allow 规则，供 `wavecode doctor` 使用。

## 审批挂起与超时拒绝

`crates/safety/gate/src/lib.rs` 提供 `ApprovalGate`：每个调用 id 一个 waiter（否则 `GateError::DuplicateWaiter`），一次性的 `decide`（已消费 id 的迟到决策返回 `false` 并被丢弃，因此过期的 UI 点击绝不可能批准未来的调用），以及 `cancel`，让到期的等待释放其 id。`QuestionGate` 为 `ask_user` 交互式提问流提供镜像实现，支持自由文本回答。

`crates/operations/bootstrap/src/gate_adapter.rs` 在门之上实现 runner 的 `ApprovalSource`：等待有截止时间；**到期解析为带明确原因的 `Deny`，绝不无限挂起**，会话中断会让挂起中的等待立即以 `Interrupted` 结束（预留被撤回，因此该 id 可重新挂起）。等待中途被丢弃的 waiter（门被清空）同样拒绝。`Headless` 变体为非交互式 driver 公开拒绝。`clear_stale` 在每个会话自有回合开始时运行（子回合跳过）。

## sandbox 判定（`crates/capabilities/sandbox/src/lib.rs`）

`Sandbox::decide(tool, input, read_only, destructive)` 按固定顺序求值：

1. **Deny 规则最先**——没有模式豁免它们，`auto` 也不行。整条复合 Bash 命令与其分段都参与匹配，因此 `echo hi\ncurl …` 无法用前缀伪装绕过 `Bash(curl *)`。命令可解析时分段是引号感知的：tree-sitter-bash 提取会忽略只在引号内被*提及*的命令，并附加一个去掉赋值的视图，因此 `X=1 curl evil` 不再能溜过 `Bash(curl *)`；不可解析的输入回退到字符串分段（覆盖面不会低于解析器之前的水平）。
2. **敏感凭据文件询问**（`.env` 家族，带文档变体豁免；SSH 私钥；`.aws`/`.gcp` 凭据存储）——在包括 `wave` 在内的所有模式下都询问，因为这道询问是注入提示悄悄读取密钥的唯一防线。点名该路径本身的精确 allow 规则（会话内"总是允许"）可豁免；通配 allow 不行。
3. **危险命令询问**（`crates/capabilities/sandbox/src/risk.rs`）——在 `auto` 与 `wave` 模式下，携带本质破坏性构造的 shell 命令会带着点名的原因询问：块设备写入（`dd of=/dev/*`）、文件系统破坏（`mkfs`、`wipefs`、分区编辑器）、电源控制（`shutdown`、`systemctl reboot`）、对系统根目录的递归强删（`rm -rf /`、`Remove-Item -Recurse -Force C:\`）、下载直通 shell（`curl … | sh`）与 fork 炸弹。检测在 tree-sitter AST 上进行（能看穿引号与 `sudo`/`env`/`nohup` 之类的间接包装）；不可解析的命令降级为对最具区分度原始 token 的文本筛查——朝着询问失败，绝不朝着沉默失败。这是 `wave` 模式下仅存的一道闸门：denylist 只覆盖用户预见到的命令，而被注入提示的破坏性命令即使在全放行模式下也应经过人眼。Plan 模式跳过该守卫（它的拒绝比任何询问更严），对同一命令文本的精确会话 allow 仍可豁免（第 2 步的非对称同样适用：持久化 grant 重载为非精确规则，下次会话询问照旧）。这张表刻意保持小而高信号——宽泛的类别属于用户自己的 denylist，而不属于一个对日常工作也喋喋不休的守卫。
4. **Allow 规则**——经规则作用域绑定到工具语义（松散的输入键嗅探不够）；对复合 Bash 命令只有*字面精确*的规则可豁免，因为通配 `*` 会跨过命令分隔符。
5. 会话内状态工具豁免（`todowrite` 与合并后的 `goal` / `plan` 工具在任何模式都无需审批——它们写的是 harness 拥有的协调状态，从不写仓库）与交互式提问路由（`ask_user`）——但 `plan` 携带 `action: "approve"` 时除外，它在所有模式下都询问：只有用户能批准提案。
6. 模式的默认策略。

`allow_always` 从被批准的调用派生一条精确的会话级 allow 规则（经 `Arc` 共享，因此克隆——包括 subagent——都看得见；deny 优先不受影响），并把同一条规则交给 grant sink，由它存储供后续会话使用。

## 启动规则从哪来（`operations-bootstrap` 的 `session::load_permissions`）

Allow 有两个来源，deny 有两个。被扩大的权限只会出自人手：

| 来源 | 文件 | 内容 |
| --- | --- | --- |
| `[permissions] allow` / `deny` | `~/.wavecode/config.toml` | 人写的规则条目，允许通配（`Bash(cargo test *)`） |
| 持久化 grant | `~/.wavecode/grants.jsonl` | 人回答"总是允许"时追加的字面条目 |
| `permission_mode` | `~/.wavecode/console-settings.json` | 启动时展示的已保存模式 |
| `wave_denylist` | `~/.wavecode/wave-denylist.json` | 裸命令片段 / `Bash(pattern)` 规则，加载时按 Bash 作用域处理；所有会话表面都加载此存储 |

两条边界防止持久化的一半变成无意间的权限放大：

- **仅用户级。** 计划中的项目层（工作目录内的 `.wavecode/config.toml`）对这些表保持未接线：agent 能写那个文件，因此 repo 范围的 allow 表会让会话给自己发放未来的豁免。
- **Grant 是字面量。** `add_grant` 拒绝任何携带 `*` 或 `?` 的条目。派生规则在其被批准的会话内按*字面*比较；把它存下来、下次加载时再当配置条目解析，会把被批准的文本无声地升级为通配 allow 面。恰好含 glob 字符的被批准命令在当次会话剩余时间内仍然豁免（带 `tracing::warn!`），想要通配的人自己去配置文件里写。

条目逐条校验（每行一次 `Rule::parse`）：一条无效只损失它自己，并作为启动发现（startup finding）浮出，绝不是被丢弃的表——因为 allow 行里的一个拼写错误丢掉整张 deny 表会无声地放大权限。一个刻意的非对称：持久化的 `File(...)` grant 重新加载为非精确规则，因此它永远不会豁免敏感凭据询问（上文第 2 步要求*精确*规则）——批准一次读 `.env` 买不来永久许可，下个会话询问照旧。`wavecode doctor` 汇报 grant 与规则（`permissions: …`），还会标记被 deny 规则可证明遮蔽的 allow 规则（`Rule::is_covered_by`，保守：可能漏报死规则，绝不凭空捏造），`wavecode grants list|remove <i>|clear` 读取并撤销 grant 表（撤销以先写后改名的方式重写文件，崩溃不可能让它截断成"无 grant"）。`PolicyAdapter`（`crates/operations/bootstrap/src/policy_adapter.rs`）把裁决映射到 runner 接缝，属性来自注册表。

## OS sandbox：fail-closed 链（`crates/capabilities/sandbox/src/chain.rs`）

`SandboxBackend` 暴露 `is_available` / `backend_name` / `enforcement` / `spawn_confined`。`PROBE_ORDER` 是 `["bwrap", "landlock", "seatbelt", "job"]`——第一个可用的获胜（`first_available`）。全都不可用时，链终结于 `UnavailableBackend`，它**拒绝所有 spawn**：fail-closed 意味着不确定 ⇒ 拒绝。`status_line` 把 `SANDBOX_UNAVAILABLE` 渲染为可 grep 的 token，标记不可用的后端。

隔离强度的诚实声明（`EnforcementLevel::Full | Partial`）：

| 后端 | 级别 | 范围 |
| --- | --- | --- |
| `bwrap`（`src/bwrap.rs`） | Full | bubblewrap，带挂载控制 |
| Landlock（`src/os.rs`） | Partial | 内核 ruleset，有已知缺口 |
| seatbelt（`src/seatbelt.rs`） | Partial | macOS `sandbox-exec` profile 缺口已记录 |
| Windows job object（`src/windows.rs`） | Partial | **仅**进程树生命周期（`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`）+ 活跃进程数上限 |

Windows 后端明确陈述其限制（`WINDOWS_UNAVAILABLE_REASON`）：没有文件系统写边界、没有网络策略、没有完整性级/AppContainer 隔离——超出其范围的隔离请求 fail-closed，而不是假装成功。`Partial` 是诚实的说法。

## 环境清洗与路径守卫

`crates/capabilities/tools/src/shell_tool.rs::sanitize_env` 在每次 spawn 前剥离：(1) 显式的 `ToolCtx::deny_env` 清单（装配时注入供应商 key 名），以及 (2) 敏感形态回退，按名字的段与后缀匹配（`_KEY`、`_PAT`、`AWS_SECRET_ACCESS_KEY` 一类）——纯后缀清单会漏掉真实形态，所以两者都检查。

文件系统围栏是 `path_guard::resolve`（基于 canonicalize 的前缀检查；见 `docs/subsystems/tools.md`，中文版 [tools.zh.md](tools.zh.md)）。面向转录与日志的密钥脱敏位于 `crates/safety/secrets`（`redact`，最长的值优先）与 `crates/foundation/auth`（`Credential` 的手写 `Debug` 输出 `REDACTED`，绝不输出密钥材料）。
