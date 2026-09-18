//! Turn-completion desktop notifications through the terminal.
//!
//! One sequence per finished turn. Delivery style follows
//! `WAVECODE_NOTIFY_STYLE`: `osc9` (default) sends an OSC 9 desktop
//! notification — supported by Windows Terminal, WezTerm, kitty,
//! ghostty, and iTerm2, silently ignored elsewhere — `bell` falls back
//! to a bare BEL ring, `both` sends the two together. Under tmux the
//! OSC 9 payload rides a DCS passthrough so the outer terminal sees
//! it. Disabled entirely with `WAVECODE_NOTIFY=0`.

/// The module id used in notification payloads.
const APP: &str = "WaveCode";

/// How a notification reaches the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    /// OSC 9 desktop notification.
    Osc9,
    /// Bare BEL ring.
    Bell,
    /// OSC 9 notification plus BEL ring.
    Both,
}

/// True when notifications are on (`WAVECODE_NOTIFY` unset or not "0").
pub fn enabled() -> bool {
    std::env::var_os("WAVECODE_NOTIFY")
        .map(|value| value != "0")
        .unwrap_or(true)
}

/// The configured delivery style (`WAVECODE_NOTIFY_STYLE`); unknown
/// values fall back to the default (`osc9`).
pub fn style() -> Style {
    parse_style(&std::env::var("WAVECODE_NOTIFY_STYLE").unwrap_or_default())
}

/// Parse a delivery-style name (`osc9` / `bell` / `both`,
/// case-insensitive); anything else is the `osc9` default.
pub fn parse_style(name: &str) -> Style {
    match name.to_lowercase().as_str() {
        "bell" => Style::Bell,
        "both" => Style::Both,
        _ => Style::Osc9,
    }
}

/// True when running inside tmux (the OSC 9 payload needs the DCS
/// passthrough to reach the outer terminal).
fn in_tmux() -> bool {
    std::env::var_os("TMUX").is_some_and(|v| !v.is_empty())
}

/// The OSC 9 sequence for one notification. Control characters are
/// stripped: the payload crosses the terminal as plain text.
pub fn sequence(body: &str) -> String {
    let clean: String = body
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    format!("\x1b]9;{APP}: {clean}\x07")
}

/// Wrap one terminal sequence in a tmux DCS passthrough (doubling the
/// ESC bytes inside) so it reaches the outer terminal instead of being
/// swallowed by the multiplexer.
fn tmux_passthrough(sequence: &str) -> String {
    let escaped = sequence.replace('\x1b', "\x1b\x1b");
    format!("\x1bPtmux;{escaped}\x1b\\")
}

/// The full notification output for one finished turn: reads the
/// configured style and tmux state from the environment.
pub fn notification(body: &str) -> String {
    notification_with(body, style(), in_tmux())
}

/// Compose the notification output from explicit inputs (the testable
/// core of [`notification`]).
pub fn notification_with(body: &str, style: Style, tmux: bool) -> String {
    let osc9 = matches!(style, Style::Osc9 | Style::Both);
    let bell = matches!(style, Style::Bell | Style::Both);
    let mut out = String::new();
    if osc9 {
        let seq = sequence(body);
        if tmux {
            out.push_str(&tmux_passthrough(&seq));
        } else {
            out.push_str(&seq);
        }
    }
    if bell {
        out.push('\x07');
    }
    out
}

/// Write a raw terminal sequence straight to stdout, bypassing the
/// diff renderer (invisible control sequences move no cursor). No
/// enablement check: callers like `/copy` act on explicit user intent.
pub fn emit_raw(sequence: &str) {
    use std::io::Write as _;
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(sequence.as_bytes());
    let _ = out.flush();
}

/// Write one notification to the terminal. No-op when notifications
/// are disabled.
pub fn emit(body: &str) {
    if !enabled() {
        return;
    }
    emit_raw(&notification(body));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_carries_app_and_body() {
        let seq = sequence("turn finished");
        assert!(seq.starts_with("\x1b]9;WaveCode: turn finished\x07"));
    }

    #[test]
    fn sequence_strips_control_characters() {
        let seq = sequence("two\r\nlines");
        assert_eq!(seq, "\x1b]9;WaveCode: two  lines\x07");
    }

    #[test]
    fn osc9_style_sends_the_notification() {
        let out = notification_with("done", Style::Osc9, false);
        assert_eq!(out, "\x1b]9;WaveCode: done\x07");
    }

    #[test]
    fn bell_style_rings_only_the_bell() {
        let out = notification_with("done", Style::Bell, false);
        assert_eq!(out, "\x07");
    }

    #[test]
    fn both_style_sends_notification_then_bell() {
        let out = notification_with("done", Style::Both, false);
        assert_eq!(out, "\x1b]9;WaveCode: done\x07\x07");
    }

    #[test]
    fn osc9_wraps_in_tmux_passthrough_under_tmux() {
        let out = notification_with("done", Style::Osc9, true);
        assert_eq!(
            out, "\x1bPtmux;\x1b\x1b]9;WaveCode: done\x07\x1b\\",
            "inner ESC doubled, wrapped in DCS ... ST"
        );
    }

    #[test]
    fn style_names_parse_case_insensitively() {
        assert_eq!(parse_style(""), Style::Osc9);
        assert_eq!(parse_style("osc9"), Style::Osc9);
        assert_eq!(parse_style("BELL"), Style::Bell);
        assert_eq!(parse_style("Both"), Style::Both);
        assert_eq!(parse_style("carrier-pigeon"), Style::Osc9);
    }
}
