//! 终端主题色与宽度工具（阶段 4 拆分自 render.rs）。
//!
//! 按字符截断（`truncate_chars`）已并入双前端单一事实源
//! `wavecode_tui::text`（render/mod.rs 再导出）。

/// 终端宽度：terminal_size 不可用时回退 80
pub(super) fn terminal_width() -> usize {
    terminal_size::terminal_size()
        .map(|(w, _)| w.0 as usize)
        .unwrap_or(80)
}

/// 工具名/波形主题色（亮青）
pub(super) fn theme_tool() -> anstyle::Style {
    anstyle::Style::new().fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::BrightCyan)))
}

/// 弱化文本（input 摘要、tokens 行）
pub(super) fn theme_dim() -> anstyle::Style {
    anstyle::Style::new().fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::BrightBlack)))
}

/// 警告（黄）
pub(super) fn theme_warn() -> anstyle::Style {
    anstyle::Style::new().fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::Yellow)))
}

/// 错误（红）
pub(super) fn theme_err() -> anstyle::Style {
    anstyle::Style::new().fg_color(Some(anstyle::Color::Ansi(anstyle::AnsiColor::Red)))
}
