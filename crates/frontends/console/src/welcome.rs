//! The welcome card shown at session start.

use crate::theme::{self, Token};
use tui_engine::border;
use tui_engine::component::Component;

/// Session facts the welcome card displays.
#[derive(Debug, Clone)]
pub struct WelcomeInfo {
    /// Product version string.
    pub version: String,
    /// Model display name.
    pub model: String,
    /// Permission mode display label.
    pub mode: String,
    /// Working directory.
    pub cwd: String,
    /// Connected MCP server names (empty when none).
    pub mcp_servers: Vec<String>,
}

/// Build the welcome card lines at `width`.
pub fn render(info: &WelcomeInfo, width: usize) -> Vec<String> {
    let theme = theme::current();
    // Two-row block mark (original design, terminal-native).
    let logo = [logo_top(), logo_bottom()];
    let mut content: Vec<String> = Vec::new();
    for row in logo {
        content.push(theme.paint(Token::Primary, &row));
    }
    content.push(theme.bold(Token::TextStrong, "WaveCode"));
    content.push(String::new());

    let dir = shorten_home(&info.cwd);
    let rows = [
        ("Directory", dir),
        ("Model", info.model.clone()),
        ("Mode", info.mode.clone()),
        ("Version", info.version.clone()),
    ];
    for (label, value) in rows {
        content.push(format!(
            "{} {}",
            theme.paint(Token::TextDim, &format!("{label}:")),
            theme.paint(Token::Text, &value)
        ));
    }
    if !info.mcp_servers.is_empty() {
        content.push(format!(
            "{} {}",
            theme.paint(Token::TextDim, "MCP:"),
            theme.paint(Token::Text, &format!("{} servers", info.mcp_servers.len()))
        ));
    }

    let mut card = border::frame(content, width, theme.style(Token::Primary), None);
    // The frame adds top and bottom borders; drop-in blank spacer keeps
    // the transcript below from hugging the card.
    card.push(String::new());
    card
}

/// The top row of the block mark.
fn logo_top() -> String {
    "▛▀▀█▀▀▜".to_string()
}

/// The bottom row of the block mark.
fn logo_bottom() -> String {
    "▙▄▄█▄▄▟".to_string()
}

/// Replace the home directory prefix with `~` when present.
fn shorten_home(path: &str) -> String {
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))
        && !home.is_empty()
        && let Some(rest) = path.strip_prefix(&home.to_string_lossy().to_string())
    {
        let rest = rest.trim_start_matches(['/', '\\']);
        return format!("~/{rest}");
    }
    path.to_string()
}

/// A component wrapper for the welcome card (renders once, cached).
pub struct Welcome {
    info: WelcomeInfo,
    width: usize,
    lines: Option<Vec<String>>,
}

impl Welcome {
    /// A card for `info`.
    pub fn new(info: WelcomeInfo) -> Self {
        Self {
            info,
            width: 0,
            lines: None,
        }
    }
}

impl Component for Welcome {
    fn render(&mut self, width: usize) -> Vec<String> {
        if self.lines.is_none() || self.width != width {
            self.width = width;
            self.lines = Some(render(&self.info, width));
        }
        self.lines.clone().unwrap_or_default()
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

    fn info() -> WelcomeInfo {
        WelcomeInfo {
            version: "0.1.0".to_string(),
            model: "test-model".to_string(),
            mode: "Ask When Needed".to_string(),
            cwd: "/tmp/project".to_string(),
            mcp_servers: vec!["a".to_string()],
        }
    }

    #[test]
    fn card_shows_brand_and_info_rows() {
        theme::set(theme::Theme::dark());
        let mut card = Welcome::new(info());
        let lines = card.render(60);
        let text: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
        let joined = text.join("\n");
        assert!(joined.contains("WaveCode"), "{joined}");
        assert!(joined.contains("Directory: /tmp/project"), "{joined}");
        assert!(joined.contains("Model: test-model"), "{joined}");
        assert!(joined.contains("MCP: 1 servers"), "{joined}");
        assert!(joined.contains("Version: 0.1.0"), "{joined}");
    }

    #[test]
    fn card_uses_rounded_primary_frame() {
        theme::set(theme::Theme::dark());
        let mut card = Welcome::new(info());
        let lines = card.render(60);
        assert!(strip_ansi(&lines[0]).starts_with('╭'));
        assert!(
            lines[0].contains("\x1b[38;2;79;168;255m"),
            "primary border: {:?}",
            lines[0]
        );
    }
}
