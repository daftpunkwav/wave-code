//! Terminal control: raw mode, bracketed paste, keyboard enhancement,
//! and background-color detection.
//!
//! A thin ownership layer over crossterm: the guard enables the modes a
//! live UI needs and restores them on drop. Inline mode only — this
//! engine never touches the alternate screen, so native scrollback
//! survives the session.

use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, EnableBracketedPaste, EnableFocusChange,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{Clear, ClearType};

/// Guard owning the terminal modes for the UI lifetime.
pub struct TerminalGuard {
    keyboard_enhanced: bool,
    bracketed_paste: bool,
    focus_reporting: bool,
}

impl TerminalGuard {
    /// Enter raw mode and enable bracketed paste, focus reporting, and
    /// (when supported) the Kitty keyboard protocol. Restores everything
    /// on `leave`/drop.
    pub fn enter() -> std::io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        let bracketed_paste = crossterm::execute!(std::io::stdout(), EnableBracketedPaste).is_ok();
        let focus_reporting = crossterm::execute!(std::io::stdout(), EnableFocusChange).is_ok();
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
        // crossterm's execute! flushes internally; nothing further needed.
        Ok(Self {
            keyboard_enhanced,
            bracketed_paste,
            focus_reporting,
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

    /// True when focus reporting is active (focus changes arrive as
    /// `Event::FocusGained`/`Event::FocusLost`).
    pub fn focus_reporting(&self) -> bool {
        self.focus_reporting
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
        if self.focus_reporting {
            let _ = crossterm::execute!(std::io::stdout(), DisableFocusChange);
            self.focus_reporting = false;
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

/// Restore terminal modes on panic, before the previous panic hook runs.
///
/// The guard's `drop` restores modes during unwinding, but the panic
/// hook fires first — without this, the panic message prints in raw
/// mode (unreadable), and an aborting panic skips `drop` entirely.
/// Also covers surfaces that never enter raw mode: both restores are
/// no-ops then, and a non-terminal stdout (exec `--json` pipe) is left
/// byte-clean. Chains to the previously installed hook; installing
/// more than once is safe but pointless — call it once at startup.
pub fn install_panic_restore() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal_now();
        previous(info);
    }));
}

/// Drop back to cooked mode and show the cursor, unconditionally.
fn restore_terminal_now() {
    use std::io::IsTerminal as _;
    if !std::io::stdout().is_terminal() {
        return;
    }
    let _ = crossterm::terminal::disable_raw_mode();
    let _ = crossterm::execute!(std::io::stdout(), crossterm::cursor::Show);
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
/// Sends `ESC]11;? BEL` and polls stdin for the reply
/// (`ESC]11;rgb:rr/gg/bb BEL`), which crossterm's event parser does not
/// surface. The poll never blocks past the timeout and never spawns a
/// reader thread: a blocking byte-read on the console input would race
/// crossterm's event reader for keystrokes (stolen input, then a starved
/// event stream). Unix only — Windows console input cannot be
/// byte-polled safely, so callers there fall back to `COLORFGBG`/dark.
/// Best-effort: `None` on any failure. Must be called while raw mode is
/// active, before the event loop starts, at most once per process.
pub fn query_background(timeout_ms: u64) -> Option<Background> {
    probe_background(timeout_ms)
}

#[cfg(unix)]
fn probe_background(timeout_ms: u64) -> Option<Background> {
    use std::io::{IsTerminal as _, Read as _, Write as _};
    use std::time::{Duration, Instant};

    static PROBED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if PROBED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return None;
    }
    // Piped stdin can never carry the reply; skip the wait entirely.
    if !std::io::stdin().is_terminal() {
        return None;
    }

    // Ask before polling: without the query no compliant terminal ever
    // answers, and the probe would just time out.
    {
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(b"\x1b]11;?\x07");
        let _ = stdout.flush();
    }

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut reply: Vec<u8> = Vec::new();
    let mut byte = [0u8; 1];
    while reply.len() < 128 {
        let now = Instant::now();
        if now >= deadline {
            return None;
        }
        let mut poll_fd = libc::pollfd {
            fd: 0, // stdin, terminal-checked above
            events: libc::POLLIN,
            revents: 0,
        };
        let remaining = (deadline - now).as_millis() as i32;
        let ready = unsafe { libc::poll(&mut poll_fd, 1, remaining.max(1)) };
        if ready <= 0 {
            return None; // timeout or poll error: give up, read nothing
        }
        match std::io::stdin().read(&mut byte) {
            Ok(1) => {
                reply.push(byte[0]);
                let terminated = byte[0] == 0x07 || reply.ends_with(&[0x1b, b'\\']);
                if terminated {
                    break;
                }
            }
            _ => return None,
        }
    }
    parse_osc11_reply(&reply)
}

#[cfg(not(unix))]
fn probe_background(_timeout_ms: u64) -> Option<Background> {
    // No portable way to byte-poll Windows console input without racing
    // crossterm's event reader; theme detection falls back to
    // `COLORFGBG`/dark (users can `/theme` at runtime).
    None
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

    /// Installing the restore hook twice must not corrupt the hook
    /// chain, and panics must still unwind as failures.
    #[test]
    fn panic_restore_install_is_repeatable_and_panics_still_fail() {
        let previous = std::panic::take_hook();
        // Silence the chained output during the test, then restore it
        // afterwards so sibling tests keep their usual reporting.
        std::panic::set_hook(Box::new(|_| {}));
        install_panic_restore();
        install_panic_restore();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            panic!("hook smoke test");
        }));
        std::panic::set_hook(previous);
        assert!(result.is_err());
    }
}
