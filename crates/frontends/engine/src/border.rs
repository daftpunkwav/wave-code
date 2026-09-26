//! Rounded-box drawing: borders, labels, and scroll indicators.
//!
//! Shared by the editor frame, welcome card, and dialogs so every boxed
//! surface in the UI shares one geometry.

use crate::color::Style;
use crate::width;

/// Draw a rounded border around content lines.
///
/// `content` holds already-styled inner lines; each is truncated/padded
/// to the interior body width. Interior geometry at total width W:
/// verticals at columns 0 and W-1, one space of padding each side,
/// body of W-4 columns. Below the minimum usable width the content
/// degrades to unframed lines.
pub fn frame(
    content: Vec<String>,
    total_width: usize,
    border: Style,
    label: Option<String>,
) -> Vec<String> {
    let inner_width = total_width.saturating_sub(2); // between the verticals
    if inner_width < 4 {
        return content;
    }
    let body_width = inner_width - 2;
    let top_mid = match label {
        Some(text) => {
            let text = width::truncate_to_width(&text, inner_width.saturating_sub(4));
            let pad = inner_width.saturating_sub(3 + width::width(&text));
            format!("─ {text} {}", "─".repeat(pad))
        }
        None => "─".repeat(inner_width),
    };
    let mut out = Vec::with_capacity(content.len() + 2);
    out.push(border.paint(format!("╭{top_mid}╮").as_str()));
    for line in content {
        let body = width::pad_to_width(&width::truncate_to_width(&line, body_width), body_width);
        let mut row = String::with_capacity(total_width);
        row.push_str(&border.paint("│"));
        row.push(' ');
        row.push_str(&body);
        row.push(' ');
        row.push_str(&border.paint("│"));
        out.push(row);
    }
    out.push(border.paint(format!("╰{}╯", "─".repeat(inner_width)).as_str()));
    out
}

/// A centered scroll indicator row for a box border: `── ↑ N more ──`.
/// The row rides `style` (the caller's dim hint tone): unpainted text
/// would inherit the terminal's default foreground, which a recolored
/// (light) background leaves unreadable.
pub fn scroll_label(total_width: usize, marker: &str, count: usize, style: Style) -> String {
    let inner = total_width.saturating_sub(2);
    if inner < 8 {
        return style.paint(&"\u{2500}".repeat(inner));
    }
    let text = format!("{} {} more", marker, count);
    let text = width::truncate_to_width(&text, inner.saturating_sub(4));
    let pad = inner.saturating_sub(2 + width::width(&text));
    let left = (pad / 2).max(1);
    let right = pad.saturating_sub(left).max(1);
    style.paint(&format!(
        "{} {} {}",
        "\u{2500}".repeat(left),
        text,
        "\u{2500}".repeat(right)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::width::strip_ansi;

    #[test]
    fn frame_draws_rounded_box_with_sides() {
        let framed = frame(vec!["hello".to_string()], 10, Style::new(), None);
        assert_eq!(framed.len(), 3);
        assert_eq!(strip_ansi(&framed[0]), "╭────────╮");
        assert_eq!(strip_ansi(&framed[1]), "│ hello  │");
        assert_eq!(strip_ansi(&framed[2]), "╰────────╯");
    }

    #[test]
    fn frame_label_sits_on_top_border() {
        let framed = frame(
            vec!["x".to_string()],
            16,
            Style::new(),
            Some("tag".to_string()),
        );
        assert_eq!(strip_ansi(&framed[0]), "╭─ tag ────────╮");
        assert_eq!(strip_ansi(&framed[1]).chars().count(), 16);
    }

    #[test]
    fn frame_degrades_below_minimum_width() {
        let framed = frame(vec!["longer content".to_string()], 4, Style::new(), None);
        assert_eq!(framed, vec!["longer content".to_string()]);
    }

    #[test]
    fn frame_truncates_overlong_body() {
        let framed = frame(vec!["0123456789".to_string()], 10, Style::new(), None);
        assert_eq!(strip_ansi(&framed[1]), "│ 012345 │");
    }

    #[test]
    fn scroll_label_centers_marker() {
        let label = scroll_label(20, "↑", 7, Style::new());
        assert_eq!(strip_ansi(&label), "──── ↑ 7 more ────");
    }
}
