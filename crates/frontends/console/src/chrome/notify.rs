//! Turn-completion desktop notifications through the terminal.
//!
//! One OSC 9 sequence per finished turn; terminals that support it
//! (Windows Terminal, WezTerm, kitty, ghostty, iTerm2) surface it as a
//! desktop notification, everything else silently ignores it. Disabled
//! entirely with `WAVECODE_NOTIFY=0`.

/// The module id used in notification payloads.
const APP: &str = "WaveCode";

/// True when notifications are on (`WAVECODE_NOTIFY` unset or not "0").
pub fn enabled() -> bool {
    std::env::var_os("WAVECODE_NOTIFY")
        .map(|value| value != "0")
        .unwrap_or(true)
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
    emit_raw(&sequence(body));
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
}
