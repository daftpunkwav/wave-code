//! Terminal text utilities: control-character sanitizing and char-based truncation (single source for both frontends).
//!
//! Shared by the terminal frontends (previously hand-synced mirror copies
//! merged here). Model /
//! tool-sourced text must pass [`sanitize_terminal`] before reaching the terminal: guards against ANSI / OSC
//! injection wiping the scrollback.

use std::borrow::Cow;

/// Whether this is a control character to strip: C0 (keeping `\n` / `\t`), DEL, C1 (U+0080-U+009F).
fn is_control(c: char) -> bool {
    matches!(c, '\u{0}'..='\u{8}' | '\u{b}'..='\u{1f}' | '\u{7f}'..='\u{9f}')
}

/// Sanitize terminal output: strip C0/C1 control characters and ESC sequences (keeping `\n`, `\t`).
/// Return a borrow with zero copy when no control characters exist.
pub fn sanitize_terminal(s: &str) -> Cow<'_, str> {
    // Fast path: borrow directly when nothing needs stripping.
    if !s.chars().any(is_control) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\x1b' {
            // Skip ESC sequences wholesale: CSI (ESC [ ... final byte 0x40-0x7E),
            // OSC (ESC ] ... terminated by BEL or ESC \), the rest as ESC plus one char.
            match it.next() {
                Some('[') => {
                    for c in it.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    for c in it.by_ref() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\x1b' {
                            // Assume ST (ESC \): swallow one more char.
                            it.next();
                            break;
                        }
                    }
                }
                // ESC-plus-one-char sequences (including lone ESC \): the skipped char is consumed.
                _ => {}
            }
            continue;
        }
        if !is_control(c) {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

/// Truncate by character count (not bytes, never splitting UTF-8); overlong input ends with an ellipsis `…`.
pub fn truncate_chars(s: &str, max: usize) -> String {
    // Zero budget holds no glyph (not even the ellipsis marker).
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
    t.push('…');
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lock sanitizing semantics (single shared impl for both frontends).
    #[test]
    fn sanitize_strips_control_sequences() {
        assert_eq!(sanitize_terminal("a\x1b[2Jb"), "ab");
        assert_eq!(sanitize_terminal("\x1b[1;31mred\x1b[0m"), "red");
        assert_eq!(sanitize_terminal("x\x1b]52;;cGF5bG9hZA==\x07y"), "xy");
        assert_eq!(sanitize_terminal("x\x1b]0;title\x1b\\y"), "xy");
        assert_eq!(sanitize_terminal("p\x07q\x08r"), "pqr");
        assert_eq!(sanitize_terminal("a\u{9b}1;31mb"), "a1;31mb");
        let s = "normal text🦀\nnewline\ttab";
        let sanitized = sanitize_terminal(s);
        assert_eq!(sanitized, s);
        assert!(
            matches!(sanitized, Cow::Borrowed(_)),
            "should borrow without copying"
        );
    }

    /// Zero/one budgets: max 0 holds nothing (not even the marker);
    /// max 1 collapses any longer input to the marker alone.
    #[test]
    fn truncate_zero_and_one_budgets() {
        assert_eq!(truncate_chars("hello", 0), "");
        assert_eq!(truncate_chars("", 0), "");
        assert_eq!(truncate_chars("hello", 1), "…");
        assert_eq!(truncate_chars("éa", 1), "…");
    }

    #[test]
    fn truncate_multibyte_utf8_by_chars() {
        let t = truncate_chars(&"é".repeat(200), 80);
        assert_eq!(t.chars().count(), 80);
        assert!(t.ends_with('…'));
        assert_eq!(truncate_chars("s", 80), "s");
    }
}
