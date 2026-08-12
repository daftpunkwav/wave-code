## Git 规范

### 提交信息（Conventional Commits）

```
<type>: <subject>
```

type 用 `feat`/`fix`/`docs`/`refactor`/`chore`/`test`/`perf` 等标准类型；subject 祈使语气、≤ 50 字符，直接描述行为，不写内部阶段编号（如 P0–P9）；一个提交只做一件事。

示例：`feat: 初始化项目代码库`、`fix: 修复长会话上下文溢出`

### 分支命名

```
<type>/<kebab-case-描述>
```

type 同上，描述用 kebab-case。

示例：`feat/context-compaction`、`fix/memory-dedup`
