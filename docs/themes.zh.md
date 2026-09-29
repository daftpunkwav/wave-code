# 主题编写指南

[English](themes.md) | 中文

WaveCode 主题就是**一个 JSON 文件——没有别的**。内置主题位于
`crates/frontends/console/src/theme/themes/*.json`；你自己的主题放进
`~/.wavecode/themes/<name>.json`，就会以内置主题同级的
`/theme <name>`（文件词干即名称）变为可选主题——与 VS Code 主题
扩展用的是同一套机制。`wavecode doctor` 会校验每一个用户主题文件。

## 快速开始

把下面内容放进 `~/.wavecode/themes/ocean-night.json`：

```json
{
  "base": "deepwave",
  "description": "a deeper ocean",
  "colors": {
    "background": "#0E1A1F",
    "primary": "#38BDF8",
    "role_user": "#38BDF8"
  }
}
```

重启（或 `/reload`）后运行 `/theme ocean-night`。没有覆盖的颜色从
`base` 继承，所以一个主题可以只有三行。

## 格式

| 字段 | 类型 | 含义 |
| --- | --- | --- |
| `colors` | object | 下面 24 个 token 的任意子集，值为 `#rrggbb`。缺失的 token 从 `base` 继承。 |
| `base` | string | 起始内置主题：`dark`、`deepwave` 或 `light`（默认 `dark`）。内置文件从不使用 `base`——它们是完整的。 |
| `dark` | bool | 主题的明暗类别。可选：有显式 `background` 覆盖时按其亮度判定，否则跟随 base 的类别，再否则为 `true`。 |
| `syntax_theme` | string | 代码高亮主题：`synthwave-84`、`ocean-dark` 或 `ocean-light`。默认继承 base 的。 |
| `description` | string | 自由文本，显示在 `/theme` 选择器中名称旁边。 |

未知字段、未知颜色名、畸形颜色和未知别名都会被拒绝——一个拼写错误
绝不会无声地渲染成 base 的样子。

## 24 个颜色 token

UI 从不硬编码颜色；每个界面都请求这些语义 token 之一（见
`theme/tokens.rs`）：

- **强调色相**（一个色相承担整个 chrome）：`primary`（提示符、用户
  输入、焦点、spinner）、`accent`（暗色主题上更亮一档，亮色主题上
  更暗一档）、`code_span`、`role_user`、`border_focus`。
- **中性文本坡道**：`text`（正文）、`text_strong`（标题、强调）、
  `text_dim`（thinking、提示）、`text_muted`（小贴士、fence url）。
- **纯灰 chrome**：`neutral`（边框、分隔线）、`border`、
  `diff_gutter`、`diff_meta`。
- **语义带**（绝不作装饰用）：`success` / `diff_added`、
  `warning` / `shell_mode`、`error` / `diff_removed`、
  `diff_added_strong`、`diff_removed_strong`、`wave`（wave 模式徽章）。
- **表面**：`background`（墨色所校准的终端背景；light 主题还会把它
  应用到终端本身）、`input_bg`（用户输入的高亮条带）。

## 内置主题遵循的规则

内置主题由测试锁定；遵循同样的契约可以让你的主题保持可读：

- 正文相对 `background` 的对比度 ≥ 7:1，dim ≥ 4.5:1，muted
  ≥ 3.5:1（WCAG 相对亮度）。
- 中性坡道不带紫色调：灰就是灰。
- `role_user`、`code_span` 与 `border_focus` 跟随 `primary`；diff
  一对复用语义带——一个强调色相，其他什么都不加。

## 文件布局（面向贡献者）

主题代码是解耦的，一个文件一个职责——颜色是数据，绝不是 Rust：

| 模块 | 职责 |
| --- | --- |
| `theme/tokens.rs` | 语义契约：`Token`、`Palette`、亮度/对比度计算。不含颜色。 |
| `theme/file.rs` | `theme.json` 格式：解析、校验、解析合并、用户主题的列出/加载。 |
| `theme/builtin.rs` | 内置数据文件（`include_str!`），只解析一次；用户主题的 `base` 经它解析。 |
| `theme/active.rs` | 全局活动主题 + 绘制辅助函数。 |
| `theme/detect.rs` | 解析配置的选择 / 终端探测。 |
| `theme/syntax.rs` | 语法高亮别名。 |

新增一个内置主题 = 向 `theme/themes/` 添加一个 JSON 文件，并在
`builtin.rs` 中加上对应条目（`include_str!` 常量、`IDS` 列表、
`builtins()` 数组，以及它的 `[BuiltinTheme; N]` 长度）。其他什么都不
用改。
