//! Transcript message components: user, assistant, and status lines.

use crate::theme::{self, Token};
use tui_engine::component::Component;
use tui_engine::markdown::{Markdown, MarkdownStyle, SyntaxHighlighter};
use tui_engine::width::{self};

/// Two-space continuation indent for message bodies.
pub const MESSAGE_INDENT: &str = "  ";
/// The status bullet prefix.
pub const STATUS_BULLET: &str = "● ";
/// The user message bullet.
pub const USER_BULLET: &str = "✨ ";

/// A user message: bullet + bold role-colored text, wrapped.
#[derive(Debug, Clone)]
pub struct UserMessage {
    text: String,
    lines: Option<(usize, Vec<String>)>,
}

impl UserMessage {
    /// A message from the submitted text.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            lines: None,
        }
    }
}

impl Component for UserMessage {
    fn render(&mut self, columns: usize) -> Vec<String> {
        if let Some((cached_width, lines)) = &self.lines
            && *cached_width == columns
        {
            return lines.clone();
        }
        let theme = theme::current();
        let body_width = columns.saturating_sub(width::width(USER_BULLET));
        let mut out = Vec::new();
        for (index, line) in width::wrap_line(&self.text, body_width)
            .into_iter()
            .enumerate()
        {
            if index == 0 {
                out.push(format!(
                    "{}{}",
                    theme.bold(Token::RoleUser, USER_BULLET),
                    theme.bold(Token::RoleUser, &line)
                ));
            } else {
                out.push(format!(
                    "{}{}",
                    MESSAGE_INDENT,
                    theme.bold(Token::RoleUser, &line)
                ));
            }
        }
        out.push(String::new());
        self.lines = Some((columns, out.clone()));
        out
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

/// An assistant message rendered as markdown with a status bullet.
pub struct AssistantMessage {
    markdown: Markdown,
    text: String,
    lines: Option<(usize, Vec<String>)>,
}

impl AssistantMessage {
    /// A completed assistant message. `highlighter` plugs syntax
    /// coloring into code blocks.
    pub fn new(text: impl Into<String>, highlighter: Box<dyn SyntaxHighlighter>) -> Self {
        Self {
            markdown: Markdown::new(
                MarkdownStyle {
                    heading: theme::current().style(Token::Text),
                    code: theme::current().style(Token::Primary),
                    link: theme::current().style(Token::Primary).underline(),
                    fence: theme::current().style(Token::TextMuted),
                    quote: theme::current().style(Token::TextDim).italic(),
                    rule: theme::current().style(Token::Border),
                },
                highlighter,
            ),
            text: text.into(),
            lines: None,
        }
    }
}

impl Component for AssistantMessage {
    fn render(&mut self, columns: usize) -> Vec<String> {
        if let Some((cached_width, lines)) = &self.lines
            && *cached_width == columns
        {
            return lines.clone();
        }
        let theme = theme::current();
        let body_width = columns.saturating_sub(width::width(STATUS_BULLET));
        let mut out = Vec::new();
        for (index, line) in self
            .markdown
            .render(&self.text, body_width)
            .into_iter()
            .enumerate()
        {
            if index == 0 {
                out.push(format!("{}{line}", theme.paint(Token::Text, STATUS_BULLET)));
            } else {
                out.push(format!("{MESSAGE_INDENT}{line}"));
            }
        }
        out.push(String::new());
        self.lines = Some((columns, out.clone()));
        out
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

/// A dim status line: `● text` (errors color the bullet and text).
#[derive(Debug, Clone)]
pub struct StatusLine {
    text: String,
    is_error: bool,
}

impl StatusLine {
    /// A neutral status note.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
        }
    }

    /// An error status note.
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
        }
    }
}

impl Component for StatusLine {
    fn render(&mut self, columns: usize) -> Vec<String> {
        let theme = theme::current();
        let token = if self.is_error {
            Token::Error
        } else {
            Token::TextDim
        };
        let body_width =
            columns.saturating_sub(width::width(STATUS_BULLET) + width::width(MESSAGE_INDENT));
        let mut out = Vec::new();
        for (index, line) in width::wrap_line(&self.text, body_width)
            .into_iter()
            .enumerate()
        {
            if index == 0 {
                out.push(format!(
                    "{MESSAGE_INDENT}{}{}",
                    theme.paint(token, STATUS_BULLET),
                    theme.paint(token, &line)
                ));
            } else {
                out.push(format!(
                    "{}{}{}",
                    MESSAGE_INDENT,
                    MESSAGE_INDENT,
                    theme.paint(token, &line)
                ));
            }
        }
        out.push(String::new());
        out
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme;
    use tui_engine::width::strip_ansi;

    #[test]
    fn user_message_has_bullet_and_wraps() {
        theme::set(theme::Theme::dark());
        let mut message = UserMessage::new("hello world");
        let lines = message.render(20);
        assert!(strip_ansi(&lines[0]).starts_with("✨ hello"));
        assert!(lines.last().unwrap().is_empty(), "trailing spacer");
    }

    #[test]
    fn status_line_uses_bullet() {
        theme::set(theme::Theme::dark());
        let mut line = StatusLine::new("compacting");
        let lines = line.render(60);
        assert_eq!(strip_ansi(&lines[0]), "  ● compacting");
    }

    #[test]
    fn error_status_colors_text() {
        theme::set(theme::Theme::dark());
        let mut line = StatusLine::error("boom");
        let lines = line.render(60);
        assert!(lines[0].contains("\x1b[38;2;232;84;84m"), "{:?}", lines[0]);
    }
}
