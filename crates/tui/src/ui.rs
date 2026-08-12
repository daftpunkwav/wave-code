//! 界面绘制：消息流 / 输入框 / 状态栏三段布局 + slash 补全与审批弹层。
//!
//! 纯绘制函数（[`draw`]）：状态全在 [`App`]，这里只做只读投影
//! （例外：消息流的滚动收敛需要可视高度，follow_tail 的恢复与 scroll
//! 钳制在绘制时回写 App，见 draw_messages）。

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::app::{App, SPINNER};

fn accent() -> Style {
    Style::default().fg(Color::LightCyan)
}

fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn warn() -> Style {
    Style::default().fg(Color::Yellow)
}

/// 整帧绘制入口。
pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let chunks = Layout::vertical([
        Constraint::Min(3),    // 消息流
        Constraint::Length(3), // 输入框（含边框）
        Constraint::Length(1), // 状态栏
    ])
    .split(area);
    draw_messages(f, app, chunks[0]);
    draw_input(f, app, chunks[1]);
    draw_status(f, app, chunks[2]);
    if app.slash_visible() {
        draw_slash_popup(f, app, chunks[1]);
    }
    if app.approval.is_some() {
        draw_approval(f, app, area);
    }
}

/// 消息流：条目顺序拼接（条目间空行），尾部追加流式缓冲的纯文本预览；
/// 自动跟随底部（follow_tail），PageUp/PageDown/滚轮翻页。
fn draw_messages(f: &mut Frame, app: &mut App, area: Rect) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (i, item) in app.items.iter().enumerate() {
        if i > 0 {
            lines.push(Line::default());
        }
        lines.extend(item.lines.iter().cloned());
    }
    let streaming = app.streaming_buffer();
    if !streaming.is_empty() {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        for l in streaming.split('\n') {
            lines.push(Line::from(Span::raw(l.to_string())));
        }
    }

    // 滚动收敛：Paragraph 按 wrap 后的视觉行滚动，这里按宽度估算视觉行数
    //（词边界换行与逐字符估算略有出入，容忍 1 行内的偏差）。
    let width = area.width.max(1) as usize;
    let visual_total: usize = lines
        .iter()
        .map(|l| (l.width() / width) + 1)
        .sum::<usize>()
        .max(1);
    let height = area.height as usize;
    let max_scroll = visual_total.saturating_sub(height);
    if app.follow_tail {
        app.scroll = max_scroll;
    } else {
        app.scroll = app.scroll.min(max_scroll);
        // 翻页回到底部：恢复自动跟随。
        if app.scroll == max_scroll {
            app.follow_tail = true;
        }
    }

    let paragraph = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((app.scroll.min(u16::MAX as usize) as u16, 0));
    f.render_widget(paragraph, area);
}

/// 输入框：带边框单行编辑；文本超宽时水平窗口跟随光标。
fn draw_input(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(if app.in_turn { dim() } else { accent() })
        .title(Span::styled(" ∿ 输入 ", accent()));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let avail = inner.width.max(1) as usize;
    let chars: Vec<char> = app.input.chars().collect();
    // 水平窗口：保证光标可见（光标左侧宽度 ≥ avail 时左移窗口起点）。
    let mut start = 0;
    let mut cursor_x: usize = chars[..app.cursor.min(chars.len())]
        .iter()
        .collect::<String>()
        .width();
    while cursor_x >= avail && start < app.cursor {
        cursor_x = cursor_x.saturating_sub(chars[start].to_string().width().max(1));
        start += 1;
    }
    let mut view = String::new();
    let mut w = 0;
    for &c in &chars[start..] {
        let cw = c.to_string().width().max(1);
        if w + cw > avail {
            break;
        }
        w += cw;
        view.push(c);
    }
    f.render_widget(Paragraph::new(view), inner);
    f.set_cursor_position((inner.x + cursor_x as u16, inner.y));
}

/// 状态栏：模型 │ 权限模式 │ token 用量 │ cwd；turn 内前缀等待动画。
fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let mut spans: Vec<Span<'static>> = Vec::new();
    if app.in_turn {
        spans.push(Span::styled(SPINNER[app.spinner % SPINNER.len()], accent()));
        spans.push(Span::raw(" "));
    }
    spans.push(Span::styled(app.model_name().to_string(), accent()));
    spans.push(Span::styled(" │ ", dim()));
    spans.push(Span::styled(app.permission_mode.to_string(), warn()));
    spans.push(Span::styled(" │ ", dim()));
    let tokens = match app.tokens {
        Some((used, window)) => format!("tokens {used}/{window}"),
        None => "tokens —".to_string(),
    };
    spans.push(Span::styled(tokens, dim()));
    spans.push(Span::styled(" │ ", dim()));
    spans.push(Span::styled(app.cwd().display().to_string(), dim()));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// slash 补全弹层：锚在输入框上方，候选列表 + 选中高亮。
fn draw_slash_popup(f: &mut Frame, app: &App, input_area: Rect) {
    let candidates = app.slash_candidates();
    let height = (candidates.len() as u16 + 2).min(10);
    let width = 40.min(input_area.width.max(20));
    let area = Rect {
        x: input_area.x,
        y: input_area.y.saturating_sub(height),
        width,
        height,
    };
    f.render_widget(Clear, area);
    let selected = app.slash_selected().min(candidates.len().saturating_sub(1));
    let lines: Vec<Line<'static>> = candidates
        .iter()
        .enumerate()
        .map(|(i, c)| {
            if i == selected {
                Line::from(Span::styled(
                    format!("▸ {c}"),
                    accent().add_modifier(Modifier::REVERSED),
                ))
            } else {
                Line::from(Span::raw(format!("  {c}")))
            }
        })
        .collect();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(dim())
        .title(Span::styled(" 命令 ", dim()));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

/// 审批内联弹窗：居中，黄边框；选择态给 y/n/Esc 提示，原因态给录入行。
fn draw_approval(f: &mut Frame, app: &App, area: Rect) {
    let Some(popup) = &app.approval else {
        return;
    };
    let kind_label = match popup.kind {
        wavecode_protocol::ApprovalKind::Exec => "执行命令",
        _ => "写入文件",
    };
    let width = (area.width * 3 / 4).clamp(30, 72).min(area.width);
    let detail_rows = (popup.detail.width() / width.saturating_sub(4).max(1) as usize + 2) as u16;
    let height = (detail_rows + 7).clamp(8, area.height.saturating_sub(2).max(8));
    let popup_area = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    f.render_widget(Clear, popup_area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(warn())
        .title(Span::styled(" ⚠ 审批请求 ", warn()));
    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let mut lines = vec![
        Line::from(vec![Span::styled(format!("类型：{kind_label}"), warn())]),
        Line::from(Span::raw(popup.detail.clone())),
        Line::default(),
    ];
    if popup.reason_mode {
        lines.push(Line::from(vec![
            Span::raw("拒绝原因（回传模型）："),
            Span::styled(popup.reason.clone(), warn()),
        ]));
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "Enter 确认拒绝 │ Esc 直接拒绝",
            dim(),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            "y 放行 │ n 拒绝（附原因） │ Esc 拒绝",
            warn(),
        )));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}
