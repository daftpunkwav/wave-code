//! ANSI-aware text measurement, truncation, and wrapping.
//!
//! Terminal UI lines carry embedded SGR/OSC sequences that occupy no
//! cells. Every helper here measures *visible* width (per
//! `unicode-width`, on grapheme boundaries), so styled text aligns and
//! wraps identically to plain text. Word wrap honors break opportunities
//! at spaces and between CJK characters, and re-emits the active SGR
//! state at the start of each continuation line. Wrapped lines are NOT
//! reset-terminated; the screen layer terminates every rendered line.

use unicode_segmentation::UnicodeSegmentation;

/// One scanned token of a line: an escape sequence or a grapheme cluster.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Token<'a> {
    /// A complete ANSI escape sequence (CSI, OSC, or two-byte escape).
    Escape(&'a str),
    /// A visible grapheme cluster (or a lone unterminated ESC).
    Grapheme(&'a str),
}

/// Token iteration for components that must splice styled rows (cursor
/// markers, horizontal windows).
pub(crate) fn tokens(line: &str) -> impl Iterator<Item = Token<'_>> {
    tokenize(line).into_iter()
}

/// Split `line` into escape tokens and visible graphemes, in order.
fn tokenize(line: &str) -> Vec<Token<'_>> {
    let bytes = line.as_bytes();
    let mut tokens = Vec::new();
    let mut rest = line;
    let mut offset = 0;
    while !rest.is_empty() {
        if rest.starts_with('\x1b') {
            if let Some(len) = escape_len(bytes, offset) {
                tokens.push(Token::Escape(&rest[..len]));
                rest = &rest[len..];
                offset += len;
                continue;
            }
            // Unterminated or lone ESC: surface it as a grapheme so a
            // stray escape can never swallow the rest of the line.
            tokens.push(Token::Grapheme("\x1b"));
            rest = &rest[1..];
            offset += 1;
            continue;
        }
        let Some((_, grapheme)) = rest.grapheme_indices(true).next() else {
            break;
        };
        tokens.push(Token::Grapheme(grapheme));
        offset += grapheme.len();
        rest = &rest[grapheme.len()..];
    }
    tokens
}

/// Length of the escape sequence starting at `bytes[offset] == ESC`, or
/// `None` when the sequence is unterminated.
fn escape_len(bytes: &[u8], offset: usize) -> Option<usize> {
    debug_assert_eq!(bytes[offset], b'\x1b');
    let after = *bytes.get(offset + 1)?;
    match after {
        b'[' => {
            // CSI: parameter/intermediate bytes then a final byte 0x40-0x7E.
            let mut i = offset + 2;
            while i < bytes.len() {
                if (0x40..=0x7e).contains(&bytes[i]) {
                    return Some(i - offset + 1);
                }
                i += 1;
            }
            None
        }
        b']' => {
            // OSC: terminated by BEL or ESC \.
            let mut i = offset + 2;
            while i < bytes.len() {
                match bytes[i] {
                    0x07 => return Some(i - offset + 1),
                    0x1b if bytes.get(i + 1) == Some(&b'\\') => return Some(i - offset + 2),
                    _ => i += 1,
                }
            }
            None
        }
        // APC/DCS/SOS/PM: consumed through the terminator (BEL accepted
        // in practice, ESC \ by the spec) so zero-width marker sequences
        // (like the editor cursor marker) never count toward width.
        b'_' | b'P' | b'^' | b'X' => {
            let mut i = offset + 2;
            while i < bytes.len() {
                match bytes[i] {
                    0x07 => return Some(i - offset + 1),
                    0x1b if bytes.get(i + 1) == Some(&b'\\') => return Some(i - offset + 2),
                    _ => i += 1,
                }
            }
            None
        }
        // Two-byte escapes (ESC 7, ESC ( B, ...): anything else following
        // ESC consumes one more byte.
        _ => Some(2),
    }
}

/// Active SGR state tracked while re-styling wrapped lines. Recognized
/// parameters mirror the styles this engine emits: truecolor/256-color
/// foreground and background, bold, dim, italic, underline, and reverse.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct SgrState {
    params: Vec<String>,
}

impl SgrState {
    /// Apply one SGR sequence's parameters to the running state.
    fn apply(&mut self, sequence: &str) {
        let Some(inner) = sequence
            .strip_prefix("\x1b[")
            .and_then(|s| s.strip_suffix('m'))
        else {
            return;
        };
        if inner.is_empty() {
            self.params.clear();
            return;
        }
        let raw: Vec<&str> = inner.split(';').collect();
        let mut i = 0;
        while i < raw.len() {
            match raw[i] {
                "" | "0" => {
                    self.params.clear();
                }
                "38" | "48" => {
                    let span = match raw.get(i + 1) {
                        Some(&"2") => 5, // 38;2;r;g;b
                        Some(&"5") => 3, // 38;5;idx
                        _ => 1,
                    };
                    let end = (i + span).min(raw.len());
                    self.params.push(raw[i..end].join(";"));
                    i = end;
                    continue;
                }
                "39" | "49" => {
                    let prefix = if raw[i] == "39" { "38" } else { "48" };
                    self.params.retain(|p| !p.starts_with(prefix));
                }
                "22" => self.params.retain(|p| p != "1" && p != "2"),
                "23" => self.params.retain(|p| p != "3"),
                "24" => self.params.retain(|p| p != "4"),
                "27" => self.params.retain(|p| p != "7"),
                other => self.params.push(other.to_string()),
            }
            i += 1;
        }
    }

    /// SGR sequence restoring this state, or "" for the default state.
    fn sequence(&self) -> String {
        if self.params.is_empty() {
            String::new()
        } else {
            format!("\x1b[{}m", self.params.join(";"))
        }
    }
}

/// Visible cell width of a line, ignoring escape sequences. Zero-width
/// graphemes (combining marks, ZWJ) contribute nothing; wide graphemes
/// contribute their column count.
pub fn width(line: &str) -> usize {
    tokenize(line)
        .into_iter()
        .map(|token| match token {
            Token::Escape(_) => 0,
            Token::Grapheme(g) => unicode_width::UnicodeWidthStr::width(g),
        })
        .sum()
}

/// Remove all ANSI escape sequences, keeping visible text only.
pub fn strip_ansi(line: &str) -> String {
    tokenize(line)
        .into_iter()
        .filter_map(|token| match token {
            Token::Escape(_) => None,
            Token::Grapheme(g) => Some(g),
        })
        .collect()
}

/// Cut `line` to at most `max_width` visible columns, preserving escape
/// sequences that appear before the cut point. Returns the unchanged line
/// when it already fits.
pub fn truncate_to_width(line: &str, max_width: usize) -> String {
    if width(line) <= max_width {
        return line.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for token in tokenize(line) {
        match token {
            Token::Escape(seq) => out.push_str(seq),
            Token::Grapheme(g) => {
                let w = unicode_width::UnicodeWidthStr::width(g);
                if used + w > max_width {
                    break;
                }
                used += w;
                out.push_str(g);
            }
        }
    }
    out
}

/// Visible columns `[start, start + len)` of a line, preserving escapes.
/// Wide graphemes straddling the left edge are skipped whole. Used for
/// horizontal input scrolling inside fixed-width boxes.
pub fn slice_by_column(line: &str, start: usize, len: usize) -> String {
    let mut out = String::new();
    let mut col = 0;
    for token in tokenize(line) {
        match token {
            Token::Escape(seq) => out.push_str(seq),
            Token::Grapheme(g) => {
                let w = unicode_width::UnicodeWidthStr::width(g);
                let next = col + w;
                if col >= start && next <= start + len {
                    out.push_str(g);
                }
                col = next;
            }
        }
    }
    out
}

/// Pad `line` with spaces to exactly `total` visible columns (no-op when
/// already at or beyond `total`).
pub fn pad_to_width(line: &str, total: usize) -> String {
    let current = width(line);
    if current >= total {
        line.to_string()
    } else {
        format!("{line}{}", " ".repeat(total - current))
    }
}

/// Word-wrap one styled line to `max_width` visible columns. Escape
/// sequences carry no width; the active SGR state is re-emitted at the
/// start of every continuation line. Spaces are dropped at breaks.
/// Tabs expand to a fixed four-column indent, whether or not the line
/// actually wraps. A `max_width` of 0 yields the line unchanged; a
/// line that fits is returned unmodified so callers can cache by
/// content.
///
/// Boundary: one grapheme wider than the whole budget (a CJK cluster
/// or emoji at a 1-column budget) still lands on its own line,
/// over-wide. Cutting it would silently drop the user's text, and a
/// grapheme cannot be split; renderers clip at write time instead.
pub fn wrap_line(line: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 || (!line.contains('\t') && width(line) <= max_width) {
        return vec![line.to_string()];
    }
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut used = 0;
    let mut state = SgrState::default();

    for token in tokenize(line) {
        match token {
            Token::Escape(seq) => {
                state.apply(seq);
                current.push_str(seq);
            }
            Token::Grapheme(g) => {
                // Tabs render at a fixed four-column indent: their zero
                // display width would desync the wrap math from what
                // the terminal shows (it advances to a tab stop).
                let expanded;
                let g = if g == "\t" {
                    expanded = "    ";
                    &expanded[..4]
                } else {
                    g
                };
                let w = unicode_width::UnicodeWidthStr::width(g);
                let overflow = used + w > max_width;
                if g == " " && (used == 0 || overflow) {
                    // No leading spaces after a break; drop the space that
                    // triggered the break too.
                    continue;
                }
                if overflow && used > 0 {
                    let trimmed = current.trim_end_matches(' ').to_string();
                    current = trimmed;
                    lines.push(std::mem::take(&mut current));
                    used = 0;
                    current.push_str(&state.sequence());
                }
                used += w;
                current.push_str(g);
            }
        }
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    lines
}

/// Word-wrap multiline text: splits on newlines first — a bare LF is a
/// hard break, not a zero-width grapheme — and wraps each line to
/// `max_width`. Empty lines survive as empty entries.
pub fn wrap_text(text: &str, max_width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.split('\n') {
        out.extend(wrap_line(line, max_width));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_len_scans_csi_and_osc() {
        let csi = b"\x1b[38;2;1;2;3m";
        assert_eq!(escape_len(csi, 0), Some(csi.len()));
        let osc = b"\x1b]8;;http://x\x07";
        assert_eq!(escape_len(osc, 0), Some(osc.len()));
        let osc_st = b"\x1b]8;;\x1b\\";
        assert_eq!(escape_len(osc_st, 0), Some(osc_st.len()));
        assert_eq!(escape_len(b"\x1b[38;2", 0), None);
    }

    #[test]
    fn width_ignores_escapes_and_counts_wide() {
        assert_eq!(width("hello"), 5);
        assert_eq!(width("\x1b[1mhi\x1b[0m"), 2);
        assert_eq!(width("你好"), 4);
        assert_eq!(width("a\x1b]0;t\x07b"), 2);
    }

    #[test]
    fn apc_marker_sequences_are_zero_width() {
        // The editor cursor marker is an APC sequence; terminals swallow
        // it and it must occupy no cells.
        assert_eq!(width("\x1b_pi:c\x07"), 0);
        assert_eq!(strip_ansi("a\x1b_pi:c\x07b"), "ab");
    }

    #[test]
    fn strip_ansi_keeps_text() {
        assert_eq!(strip_ansi("\x1b[1mhi\x1b[0m"), "hi");
        assert_eq!(strip_ansi("plain"), "plain");
    }

    #[test]
    fn truncate_preserves_styles_and_width() {
        let styled = "\x1b[31mabcdef\x1b[0m";
        let cut = truncate_to_width(styled, 3);
        assert_eq!(width(&cut), 3);
        assert!(cut.starts_with("\x1b[31mabc"));
        assert_eq!(truncate_to_width("ab", 5), "ab");
    }

    #[test]
    fn slice_by_column_windows_visible_cells() {
        let line = "你好world";
        assert_eq!(width(&slice_by_column(line, 0, 4)), 4);
        // Window [2,7): 好 (cols 2-3) + w/o/r (cols 4-6).
        assert_eq!(strip_ansi(&slice_by_column(line, 2, 5)), "好wor");
        assert_eq!(slice_by_column(line, 20, 5), "");
    }

    #[test]
    fn pad_reaches_target_width() {
        assert_eq!(pad_to_width("ab", 4), "ab  ");
        assert_eq!(pad_to_width("\x1b[1mab\x1b[0m", 4), "\x1b[1mab\x1b[0m  ");
        assert_eq!(pad_to_width("abcdef", 4), "abcdef");
    }

    #[test]
    fn wrap_breaks_at_spaces() {
        let lines = wrap_line("hello world", 5);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "hello");
        assert_eq!(lines[1], "world");
    }

    #[test]
    fn wrap_reopens_style_on_continuation() {
        let lines = wrap_line("\x1b[1mhello world\x1b[0m", 5);
        assert_eq!(lines.len(), 2);
        assert_eq!(strip_ansi(&lines[0]), "hello");
        assert_eq!(strip_ansi(&lines[1]), "world");
        assert!(lines[1].starts_with("\x1b[1m"), "got: {:?}", lines[1]);
    }

    #[test]
    fn wrap_breaks_between_cjk() {
        let lines = wrap_line("你好世界", 4);
        assert_eq!(lines, vec!["你好", "世界"]);
    }

    #[test]
    fn wrap_hard_clips_long_words() {
        let lines = wrap_line("abcdefghijklmnop", 6);
        assert_eq!(lines, vec!["abcdef", "ghijkl", "mnop"]);
    }

    #[test]
    fn wrap_returns_input_when_fitting() {
        assert_eq!(wrap_line("short", 80), vec!["short".to_string()]);
        assert_eq!(wrap_line("any", 0), vec!["any".to_string()]);
    }

    #[test]
    fn wide_grapheme_in_a_tiny_budget_gets_its_own_line() {
        // A 2-column cluster at a 1-column budget cannot be split: it
        // lands alone on its line (over-wide) instead of dropping text.
        let lines = wrap_line("a你b", 1);
        assert_eq!(lines, vec!["a", "你", "b"]);
        let lines = wrap_line("👍x", 1);
        assert_eq!(strip_ansi(&lines[0]), "👍");
        assert_eq!(strip_ansi(&lines[1]), "x");
    }

    #[test]
    fn sgr_state_tracks_color_and_flags() {
        let mut state = SgrState::default();
        state.apply("\x1b[1m");
        state.apply("\x1b[38;2;1;2;3m");
        assert_eq!(state.sequence(), "\x1b[1;38;2;1;2;3m");
        state.apply("\x1b[22m");
        assert_eq!(state.sequence(), "\x1b[38;2;1;2;3m");
        state.apply("\x1b[0m");
        assert_eq!(state.sequence(), "");
    }
}
