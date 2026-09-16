//! Terminal control: raw mode, bracketed paste, keyboard enhancement,
//! and background-color detection.
//!
//! A thin ownership layer over crossterm: the guard enables the modes a
//! live UI needs and restores them on drop. Inline mode only — this
//! engine never touches the alternate screen, so native scrollback
//! survives the session.

use std::io::{Read as _, Write as _};

use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{Clear, ClearType};

/// Guard owning the terminal modes for the UI lifetime.
pub struct TerminalGuard {
    keyboard_enhanced: bool,
    bracketed_paste: bool,
}

impl TerminalGuard {
    /// Enter raw mode and enable bracketed paste plus (when supported)
    /// the Kitty keyboard protocol. Restores everything on `leave`/drop.
    pub fn enter() -> std::io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        let bracketed_paste = crossterm::execute!(std::io::stdout(), EnableBracketedPaste).is_ok();
        let mut keyboard_enhanced = false;
        if crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false) {
            keyboard_enhanced = crossterm::execute!(
                std::io::stdout(),
                PushKeyboardEnhancementFlags(
                    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                        | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                        | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS,
                )
            )
            .is_ok();
        }
        let _ = std::io::stdout().flush();
        Ok(Self {
            keyboard_enhanced,
            bracketed_paste,
        })
    }

    /// True when the Kitty keyboard protocol negotiated successfully
    /// (press/release events and alternate keys are reported).
    pub fn keyboard_enhanced(&self) -> bool {
        self.keyboard_enhanced
    }

    /// True when bracketed paste is active (paste events arrive as
    /// `Event::Paste`).
    pub fn bracketed_paste(&self) -> bool {
        self.bracketed_paste
    }

    /// Restore every mode taken by [`Self::enter`].
    pub fn leave(&mut self) {
        if self.keyboard_enhanced {
            let _ = crossterm::execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
            self.keyboard_enhanced = false;
        }
        if self.bracketed_paste {
            let _ = crossterm::execute!(std::io::stdout(), DisableBracketedPaste);
            self.bracketed_paste = false;
        }
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::cursor::Show,
            Clear(ClearType::UntilNewLine)
        );
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.leave();
    }
}

/// Current terminal size as (width, height) in cells.
pub fn size() -> std::io::Result<(usize, usize)> {
    let (columns, rows) = crossterm::terminal::size()?;
    Ok((columns as usize, rows as usize))
}

/// The detected terminal background for theme auto-detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Background {
    /// Dark background (light text).
    Dark,
    /// Light background (dark text).
    Light,
}

/// Query the terminal background color via OSC 11 with a bounded wait.
///
/// The reply arrives as raw bytes on stdin (`ESC]11;rgb:rr/gg/bb BEL`),
/// which crossterm's event parser does not surface, so this reads stdin
/// directly on a worker thread with a timeout. Best-effort: `None` on
/// any failure, and callers fall back to `COLORFGBG`/dark. Must be
/// called while raw mode is active, before the event loop starts.
pub fn query_background(timeout_ms: u64) -> Option<Background> {
    use std::sync::mpsc;
    use std::time::Duration;

    let (sender, receiver) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buffer = Vec::new();
        let mut byte = [0u8; 1];
        // Read until the BEL terminator or a bounded byte budget.
        while buffer.len() < 128 {
            match stdin.read(&mut byte) {
                Ok(1) => {
                    buffer.push(byte[0]);
                    if byte[0] == 0x07 || (byte[0] == b'\\' && buffer.ends_with(&[0x1b, b'\\'])) {
                        break;
                    }
                }
                _ => break,
            }
        }
        sender.send(buffer).ok();
    });
    let _ = &reader;
    let reply = receiver
        .recv_timeout(Duration::from_millis(timeout_ms))
        .ok()?;
    parse_osc11_reply(&reply)
}

/// Parse an `ESC]11;rgb:RRRR/GGGG/BBBB BEL` (or ST-terminated) reply
/// into light/dark by relative luminance.
pub fn parse_osc11_reply(reply: &[u8]) -> Option<Background> {
    let text = String::from_utf8_lossy(reply);
    let start = text.find("rgb:")?;
    let payload = text[start + 4..].trim_end_matches(['\x07', '\\', '\x1b']);
    let mut channels = payload.split('/');
    let parse = |raw: &str| -> Option<f64> {
        let digits: String = raw.chars().filter(|c| c.is_ascii_hexdigit()).collect();
        if digits.len() < 2 {
            return None;
        }
        // Replies use 2 or 4 hex digits per channel; normalize to 0..=1.
        let value = u32::from_str_radix(&digits, 16).ok()?;
        Some(value as f64 / (16f64.powi(digits.len() as i32) - 1.0))
    };
    let r = parse(channels.next()?)?;
    let g = parse(channels.next()?)?;
    let b = parse(channels.next()?)?;
    // Relative luminance approximation.
    let luminance = 0.299 * r + 0.587 * g + 0.114 * b;
    Some(if luminance > 0.5 {
        Background::Light
    } else {
        Background::Dark
    })
}

/// Fallback background detection from `COLORFGBG` (e.g. `15;0`), used
/// when the OSC 11 query fails.
pub fn background_from_colorfgbg(value: &str) -> Option<Background> {
    let bg = value.rsplit(';').next()?;
    let code: u32 = bg.trim().parse().ok()?;
    // Classic 16-color palette: 0-6 and 8 are dark backgrounds.
    Some(if code <= 6 || code == 8 {
        Background::Dark
    } else {
        Background::Light
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc11_dark_reply_parses() {
        let reply = b"\x1b]11;rgb:1e1e/2a2a/1e1e\x07";
        assert_eq!(parse_osc11_reply(reply), Some(Background::Dark));
    }

    #[test]
    fn osc11_light_reply_parses() {
        let reply = b"\x1b]11;rgb:ffff/ffff/ffff\x07";
        assert_eq!(parse_osc11_reply(reply), Some(Background::Light));
    }

    #[test]
    fn st_terminated_reply_parses() {
        let reply = b"\x1b]11;rgb:0000/0000/0000\x1b\\";
        assert_eq!(parse_osc11_reply(reply), Some(Background::Dark));
    }

    #[test]
    fn malformed_replies_are_none() {
        assert_eq!(parse_osc11_reply(b""), None);
        assert_eq!(parse_osc11_reply(b"\x1b]11;junk\x07"), None);
    }

    #[test]
    fn colorfgbg_fallback() {
        assert_eq!(background_from_colorfgbg("15;0"), Some(Background::Dark));
        assert_eq!(background_from_colorfgbg("0;15"), Some(Background::Light));
        assert_eq!(background_from_colorfgbg("junk"), None);
    }
}
