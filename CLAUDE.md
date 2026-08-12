## Git 规范

### 提交信息（Conventional Commits）

提交信息遵循 [Conventional Commits](https://www.conventionalcommits.org/) 规范：

```
<type>(<scope>): <subject>
```

**type 枚举：**

| type | 用途 |
|------|------|
| `feat` | 新功能 |
| `fix` | 缺陷修复 |
| `docs` | 仅文档变更 |
| `refactor` | 行为不变的重构 |
| `perf` | 性能优化 |
| `test` | 测试相关 |
| `chore` | 构建、依赖等杂务 |
| `build` | 构建系统或外部依赖变更 |
| `ci` | CI 配置变更 |

**规则：**

- subject 用祈使语气、≤ 50 字符，直接描述行为；不写内部阶段编号（如 P0–P9），不以"更新文档"之类作为提交主体
- 需要时用 body 说明动机；一个提交只做一件事
- 提交前运行格式与测试校验

**示例：**

- `feat: 初始化项目代码库`
- `fix: 修复长会话上下文溢出`
- `chore: 升级 tokio 依赖`

### 分支命名

```
<type>/<kebab-case-描述>
```

type 同提交规范，描述用 kebab-case（小写短横线连接）：

- `feat/context-compaction`
- `fix/memory-dedup`
- `chore/deps-upgrade`
