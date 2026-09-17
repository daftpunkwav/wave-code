//! Transcript message components: user, assistant, and status lines.
//!
//! While the assistant is streaming, the message bullet is a pulsing
//! sine amplitude (one character, breathing with the wave); the
//! finished message settles on the sine glyph.

use std::time::{Duration, Instant};

use crate::theme::{self, Token};
use tui_engine::component::Component;
use tui_engine::loader::SINE_FRAMES;
use tui_engine::markdown::{Markdown, MarkdownStyle, SyntaxHighlighter};
use tui_engine::width::{self};

/// Two-space continuation indent for message bodies.
pub const MESSAGE_INDENT: &str = "  ";
/// The status bullet prefix (neutral system status, not a wave kind;
/// the glyph is [`crate::chrome::symbols::DONE`] plus a space).
pub const STATUS_BULLET: &str = "● ";
/// The user message bullet: a full square-wave cycle (keyed-in pulses).
pub const USER_BULLET: &str = "⊓⊔ ";
/// The finished assistant message bullet: a sine wave glyph.
pub const ASSISTANT_BULLET: &str = "∿ ";
/// Frame interval of the streaming sine animation.
pub const STREAM_FRAME_INTERVAL: Duration = Duration::from_millis(110);

/// The pulsing streaming bullet for `elapsed`: one character of the
/// sine amplitude cycle, breathing with the wave.
fn stream_bullet(elapsed: Duration) -> &'static str {
    let step = (elapsed.as_millis() / STREAM_FRAME_INTERVAL.as_millis()) as usize;
    SINE_FRAMES[step % SINE_FRAMES.len()]
}

/// A user message: bullet + bold role-colored body. With markdown
/// enabled (the default) the body renders through the markdown engine
/// in the user role color; plain mode wraps the raw text instead.
pub struct UserMessage {
    text: String,
    markdown: Option<Markdown>,
    lines: Option<(usize, Vec<String>)>,
}

impl UserMessage {
    /// A message from the submitted text. `markdown` selects the
    /// rendering mode (the editor never renders; only the transcript
    /// does).
    pub fn new(text: impl Into<String>, markdown: bool) -> Self {
        let text = text.into();
        let markdown = markdown.then(|| {
            Markdown::new(
                MarkdownStyle {
                    heading: theme::current().style(Token::RoleUser),
                    code: theme::current().style(Token::RoleUser),
                    link: theme::current().style(Token::RoleUser).underline(),
                    fence: theme::current().style(Token::TextMuted),
                    quote: theme::current().style(Token::RoleUser).italic(),
                    rule: theme::current().style(Token::Border),
                },
                Box::new(tui_engine::markdown::PlainHighlighter),
            )
        });
        Self {
            text,
            markdown,
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
        let body: Vec<String> = match &mut self.markdown {
            Some(markdown) => markdown.render(&self.text, body_width),
            None => width::wrap_line(&self.text, body_width),
        };
        let mut out = Vec::new();
        for (index, line) in body.iter().enumerate() {
            if index == 0 {
                out.push(format!(
                    "{}{}",
                    theme.bold(Token::RoleUser, USER_BULLET),
                    theme.bold(Token::RoleUser, line)
                ));
            } else {
                out.push(format!(
                    "{}{}",
                    MESSAGE_INDENT,
                    theme.bold(Token::RoleUser, line)
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

/// An assistant message rendered as markdown with a sine bullet. Built
/// with [`AssistantMessage::streaming`] while the turn is live, the
/// bullet travels (sine window animation) until the cache is broken.
pub struct AssistantMessage {
    markdown: Markdown,
    text: String,
    live: bool,
    started: Instant,
    lines: Option<(usize, usize, usize, Vec<String>)>,
}

impl AssistantMessage {
    /// A completed assistant message. `highlighter` plugs syntax
    /// coloring into code blocks.
    pub fn new(text: impl Into<String>, highlighter: Box<dyn SyntaxHighlighter>) -> Self {
        Self {
            markdown: Self::markdown(highlighter),
            text: text.into(),
            live: false,
            started: Instant::now(),
            lines: None,
        }
    }

    /// A live assistant message whose bullet animates while streaming.
    pub fn streaming(text: impl Into<String>, highlighter: Box<dyn SyntaxHighlighter>) -> Self {
        Self {
            live: true,
            ..Self::new(text, highlighter)
        }
    }

    fn markdown(highlighter: Box<dyn SyntaxHighlighter>) -> Markdown {
        Markdown::new(
            MarkdownStyle {
                heading: theme::current().style(Token::Text),
                code: theme::current().style(Token::Primary),
                link: theme::current().style(Token::Primary).underline(),
                fence: theme::current().style(Token::TextMuted),
                quote: theme::current().style(Token::TextDim).italic(),
                rule: theme::current().style(Token::Border),
            },
            highlighter,
        )
    }

    /// Settle a live message into its final static form.
    pub fn settle(&mut self) {
        self.live = false;
        self.lines = None;
    }
}

impl Component for AssistantMessage {
    fn render(&mut self, columns: usize) -> Vec<String> {
        let frame = if self.live {
            (self.started.elapsed().as_millis() / STREAM_FRAME_INTERVAL.as_millis()) as usize
        } else {
            0
        };
        if let Some((cached_columns, cached_frame, cached_width, lines)) = &self.lines
            && *cached_columns == columns
            && *cached_frame == frame
            && *cached_width == width::width(&self.text)
        {
            return lines.clone();
        }
        let theme = theme::current();
        let body_width = columns.saturating_sub(width::width(ASSISTANT_BULLET));
        // Live: the sine amplitude pulsing in one cell. Final: the sine
        // glyph — both occupy the same two columns, text never reflows.
        let bullet = if self.live {
            stream_bullet(self.started.elapsed()).to_string()
        } else {
            format!("{ASSISTANT_BULLET} ")
        };
        let mut out = Vec::new();
        for (index, line) in self
            .markdown
            .render(&self.text, body_width)
            .into_iter()
            .enumerate()
        {
            if index == 0 {
                out.push(format!("{}{}{line}", theme.paint(Token::Primary, &bullet), " "));
            } else {
                out.push(format!("{MESSAGE_INDENT}{line}"));
            }
        }
        out.push(String::new());
        self.lines = Some((columns, frame, width::width(&self.text), out.clone()));
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
        let mut message = UserMessage::new("hello world", false);
        let lines = message.render(20);
        assert!(strip_ansi(&lines[0]).starts_with("⊓⊔ hello"));
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
